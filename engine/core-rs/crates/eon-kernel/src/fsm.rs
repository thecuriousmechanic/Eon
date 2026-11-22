//! Task state machine with an explicit transition table (docs/implementation/01, section 4).
//!
//! Every state change goes through `next_state`. Illegal transitions are errors, never silent no-ops,
//! so a corrupted or replayed event cannot move a task somewhere the table does not allow.

use eon_primitives::{EonError, EonResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskState {
    Pending = 0,
    Ready = 1,
    Running = 2,
    Completed = 3,
    Failed = 4,
    Compensating = 5,
    Compensated = 6,
    /// The side effect may have occurred. Wait for reconciliation instead of running it again.
    UncertainFailure = 7,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskEvent {
    DependenciesMet,
    Start,
    Succeed,
    Fail,
    Rollback,
    CompensationDone,
    /// Execution was interrupted or its outcome could not be verified.
    OutcomeUncertain,
    /// The reconciler has already rolled back the uncertain effect.
    ReconciledRollback,
    /// The reconciler has established that repeating the task is idempotent.
    RequeueIdempotent,
}

pub const ALL_STATES: [TaskState; 8] = [
    TaskState::Pending,
    TaskState::Ready,
    TaskState::Running,
    TaskState::Completed,
    TaskState::Failed,
    TaskState::Compensating,
    TaskState::Compensated,
    TaskState::UncertainFailure,
];

pub const ALL_EVENTS: [TaskEvent; 9] = [
    TaskEvent::DependenciesMet,
    TaskEvent::Start,
    TaskEvent::Succeed,
    TaskEvent::Fail,
    TaskEvent::Rollback,
    TaskEvent::CompensationDone,
    TaskEvent::OutcomeUncertain,
    TaskEvent::ReconciledRollback,
    TaskEvent::RequeueIdempotent,
];

/// The complete legal transition table. Anything not listed here is rejected.
pub fn next_state(from: TaskState, event: TaskEvent) -> EonResult<TaskState> {
    use TaskEvent as E;
    use TaskState as S;
    let to = match (from, event) {
        (S::Pending, E::DependenciesMet) => S::Ready,
        (S::Ready, E::Start) => S::Running,
        (S::Running, E::Succeed) => S::Completed,
        (S::Running, E::Fail) => S::Failed,
        (S::Running, E::OutcomeUncertain) => S::UncertainFailure,
        (S::UncertainFailure, E::ReconciledRollback) => S::Failed,
        (S::UncertainFailure, E::RequeueIdempotent) => S::Ready,
        (S::Failed, E::Rollback) | (S::Completed, E::Rollback) => S::Compensating,
        (S::Compensating, E::CompensationDone) => S::Compensated,
        _ => {
            return Err(EonError::ValidationError(format!(
                "illegal task transition: {from:?} on {event:?}"
            )));
        }
    };
    Ok(to)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_reaches_completed() {
        let mut s = TaskState::Pending;
        for e in [TaskEvent::DependenciesMet, TaskEvent::Start, TaskEvent::Succeed] {
            s = next_state(s, e).unwrap();
        }
        assert_eq!(s, TaskState::Completed);
    }

    #[test]
    fn failure_path_compensates_and_settles() {
        let mut s = TaskState::Pending;
        for e in [TaskEvent::DependenciesMet, TaskEvent::Start, TaskEvent::Fail, TaskEvent::Rollback, TaskEvent::CompensationDone] {
            s = next_state(s, e).unwrap();
        }
        assert_eq!(s, TaskState::Compensated);
    }

    #[test]
    fn only_listed_edges_are_legal_and_reach_the_expected_state() {
        use TaskEvent as E;
        use TaskState as S;
        let legal = [
            (S::Pending, E::DependenciesMet, S::Ready),
            (S::Ready, E::Start, S::Running),
            (S::Running, E::Succeed, S::Completed),
            (S::Running, E::Fail, S::Failed),
            (S::Running, E::OutcomeUncertain, S::UncertainFailure),
            (S::UncertainFailure, E::ReconciledRollback, S::Failed),
            (S::UncertainFailure, E::RequeueIdempotent, S::Ready),
            (S::Failed, E::Rollback, S::Compensating),
            (S::Completed, E::Rollback, S::Compensating),
            (S::Compensating, E::CompensationDone, S::Compensated),
        ];
        for state in ALL_STATES {
            for event in ALL_EVENTS {
                let expected = legal.iter().find(|&&(s, e, _)| (s, e) == (state, event));
                assert_eq!(
                    next_state(state, event).ok(),
                    expected.map(|&(_, _, to)| to),
                    "{state:?} on {event:?}"
                );
            }
        }
    }

    #[test]
    fn terminal_states_accept_no_event() {
        for event in ALL_EVENTS {
            assert!(next_state(TaskState::Compensated, event).is_err());
        }
    }

    #[test]
    fn replayed_completion_cannot_restart_a_finished_task() {
        assert!(next_state(TaskState::Completed, TaskEvent::Start).is_err());
        assert!(next_state(TaskState::Completed, TaskEvent::Succeed).is_err());
    }
}
