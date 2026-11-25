//! Kernel event emission (docs/implementation/01, section 4). Every state change the kernel makes is reported to a
//! sink before the caller sees success, so a durable sink (the group-commit journal) can record it first.

use std::collections::BTreeMap;

use eon_primitives::{EonResult, TaskId, WorkflowId};

use crate::fsm::{TaskEvent, TaskState, next_state};
use crate::workflow::{self, Task, Workflow};

fn fsm_state(state: workflow::TaskState) -> TaskState {
    match state {
        workflow::TaskState::Pending => TaskState::Pending,
        workflow::TaskState::Completed => TaskState::Completed,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KernelEvent {
    /// Binds task identities and retry policy without persisting capability parameters.
    WorkflowSubmitted { workflow: WorkflowId, epoch: u64, digest: [u8; 32], tasks: BTreeMap<TaskId, bool> },
    /// Permanently withdraws the authority of this workflow at this epoch.
    EpochInvalidated { workflow: WorkflowId, epoch: u64 },
    TaskTransition { task: TaskId, from: TaskState, to: TaskState },
    SubgraphSpliced { parent: TaskId, added: Vec<TaskId> },
    /// A cognitive epoch committed these writes. Keys and values are already scrubbed when recorded.
    EpochCommitted { id: u64, writes: BTreeMap<String, String> },
}

pub trait EventSink {
    fn record(&mut self, event: KernelEvent) -> EonResult<()>;
}

/// In-memory sink for tests and for callers that persist events later.
#[derive(Debug, Default)]
pub struct VecSink {
    pub events: Vec<KernelEvent>,
}

impl EventSink for VecSink {
    fn record(&mut self, event: KernelEvent) -> EonResult<()> {
        self.events.push(event);
        Ok(())
    }
}

/// Validates an FSM transition and records it before updating `state`.
/// Illegal transitions never reach the sink; a sink error leaves `state` unchanged.
pub fn transition_recorded(
    state: &mut TaskState,
    task: &TaskId,
    event: TaskEvent,
    sink: &mut dyn EventSink,
) -> EonResult<()> {
    let from = *state;
    let to = next_state(from, event)?;
    sink.record(KernelEvent::TaskTransition { task: task.clone(), from, to })?;
    *state = to;
    Ok(())
}

/// Splices a subgraph and records the event. The event is written only after the splice succeeds, and the graph is
/// left unchanged if recording fails.
pub fn splice_recorded(
    workflow: &mut Workflow,
    parent: &TaskId,
    children: Vec<Task>,
    sink: &mut dyn EventSink,
) -> EonResult<()> {
    let added: Vec<TaskId> = children.iter().map(|c| c.id.clone()).collect();
    let before = workflow.clone();
    workflow.splice_subgraph(parent, children)?;
    if let Err(err) = sink.record(KernelEvent::SubgraphSpliced { parent: parent.clone(), added }) {
        *workflow = before;
        return Err(err);
    }
    Ok(())
}

/// Completes a task and records the transition. The state changes only if the event is recorded.
pub fn complete_recorded(workflow: &mut Workflow, task: &TaskId, sink: &mut dyn EventSink) -> EonResult<()> {
    let from = workflow.get(task).map(|t| fsm_state(t.state)).unwrap_or(TaskState::Pending);
    let before = workflow.clone();
    workflow.complete(task)?;
    if let Err(err) = sink.record(KernelEvent::TaskTransition {
        task: task.clone(),
        from,
        to: TaskState::Completed,
    }) {
        *workflow = before;
        return Err(err);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use eon_primitives::{EonError, EonResult, TaskId};

    use super::*;
    use crate::workflow::Task;

    struct FailingSink;

    impl EventSink for FailingSink {
        fn record(&mut self, _: KernelEvent) -> EonResult<()> {
            Err(EonError::InternalError("disk full".into()))
        }
    }

    fn one_task_workflow() -> (Workflow, TaskId) {
        let mut wf = Workflow::new();
        let id = TaskId::new();
        wf.insert(Task::new(id.clone(), [])).unwrap();
        (wf, id)
    }

    #[test]
    fn uncertain_outcome_records_one_transition_and_does_not_restart() {
        let task = TaskId::new();
        let mut state = TaskState::Running;
        let mut sink = VecSink::default();
        transition_recorded(&mut state, &task, TaskEvent::OutcomeUncertain, &mut sink).unwrap();
        assert_eq!(state, TaskState::UncertainFailure);

        assert!(matches!(
            transition_recorded(&mut state, &task, TaskEvent::Start, &mut sink),
            Err(EonError::ValidationError(_))
        ));
        assert_eq!(state, TaskState::UncertainFailure);
        assert_eq!(
            sink.events,
            [KernelEvent::TaskTransition { task, from: TaskState::Running, to: TaskState::UncertainFailure }]
        );
    }

    #[test]
    fn failed_recording_does_not_release_an_uncertain_task_for_retry() {
        let mut state = TaskState::UncertainFailure;
        let result = transition_recorded(
            &mut state,
            &TaskId::new(),
            TaskEvent::RequeueIdempotent,
            &mut FailingSink,
        );
        assert!(matches!(result, Err(EonError::InternalError(message)) if message == "disk full"));
        assert_eq!(state, TaskState::UncertainFailure);
    }

    #[test]
    fn completion_emits_a_transition_event() {
        let (mut wf, id) = one_task_workflow();
        let mut sink = VecSink::default();
        complete_recorded(&mut wf, &id, &mut sink).unwrap();
        assert_eq!(
            sink.events,
            vec![KernelEvent::TaskTransition { task: id.clone(), from: TaskState::Pending, to: TaskState::Completed }]
        );
        assert_eq!(wf.get(&id).unwrap().state, workflow::TaskState::Completed);
    }

    #[test]
    fn failed_recording_rolls_back_the_completion() {
        let (mut wf, id) = one_task_workflow();
        let before = wf.clone();
        assert!(complete_recorded(&mut wf, &id, &mut FailingSink).is_err());
        assert_eq!(wf, before, "state must not change without a durable record");
    }

    #[test]
    fn splice_emits_event_listing_the_new_tasks() {
        let (mut wf, parent) = one_task_workflow();
        complete_recorded(&mut wf, &parent, &mut VecSink::default()).unwrap();
        let child = TaskId::new();
        let mut sink = VecSink::default();
        splice_recorded(&mut wf, &parent, vec![Task::new(child.clone(), [])], &mut sink).unwrap();
        assert_eq!(sink.events, vec![KernelEvent::SubgraphSpliced { parent, added: vec![child] }]);
    }

    #[test]
    fn failed_splice_recording_leaves_the_graph_unchanged() {
        let (mut wf, parent) = one_task_workflow();
        complete_recorded(&mut wf, &parent, &mut VecSink::default()).unwrap();
        let before = wf.clone();
        let child = TaskId::new();
        assert!(splice_recorded(&mut wf, &parent, vec![Task::new(child, [])], &mut FailingSink).is_err());
        assert_eq!(wf, before);
    }
}
