//! Durable workflow scheduling over the kernel journal.
//!
//! The server runs no task executors. The operator submits a workflow, asks for dispatches, and reports
//! each attempt's outcome. Every scheduler transition is written to the journal and fsynced before the
//! call returns, because [`JournalWriter`] forwards each event to [`JournalSink`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use eon_kernel::events::{EventSink, KernelEvent};
use eon_kernel::fsm::TaskState;
use eon_kernel::journal_sink::JournalSink;
use eon_kernel::model::{Task, TaskSpec, TaskType, Workflow};
use eon_kernel::recovery::LocalExecutionRecoveryWorker;
use eon_kernel::resource::{
    CanonicalResourceKey, Demand, Requirement, ResourceManager, ResourceRank,
};
use eon_kernel::scheduler::{Countermand, Dispatch, Outcome, TaskScheduler};
use eon_primitives::{EonError, EonResult, ObjectiveId, TaskId, WorkflowId};
use serde::{Deserialize, Serialize};

pub const MAX_TASKS: usize = 32;
const MAX_LABEL_BYTES: usize = 64;
const MAX_TIMEOUT_MS: u64 = 3_600_000;

#[derive(Clone)]
pub struct JournalWriter(Arc<Mutex<LocalExecutionRecoveryWorker>>);

impl EventSink for JournalWriter {
    fn record(&mut self, event: KernelEvent) -> EonResult<()> {
        let mut worker = self.0.lock().map_err(|_| poisoned())?;
        let (journal, _) = worker.parts();
        JournalSink::new(journal).record(event)
    }
}

fn poisoned() -> EonError {
    EonError::OperationFailed {
        operation: "kernel.journal".into(),
        reason: "journal worker lock poisoned by an earlier panic".into(),
    }
}

#[derive(Debug, Deserialize)]
pub struct WorkflowRequest {
    pub tasks: Vec<TaskRequest>,
}

#[derive(Debug, Deserialize)]
pub struct TaskRequest {
    pub label: String,
    pub capability: String,
    pub operation: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default = "one_core")]
    pub cpu_cores: u64,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub is_idempotent: bool,
}

fn one_core() -> u64 {
    1
}

fn default_timeout_ms() -> u64 {
    60_000
}

#[derive(Debug, Deserialize)]
pub struct FinishRequest {
    pub label: String,
    pub outcome: OutcomeName,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeName {
    Succeeded,
    Failed,
    Uncertain,
}

impl From<OutcomeName> for Outcome {
    fn from(name: OutcomeName) -> Self {
        match name {
            OutcomeName::Succeeded => Outcome::Succeeded,
            OutcomeName::Failed => Outcome::Failed,
            OutcomeName::Uncertain => Outcome::Uncertain,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct KernelView {
    pub workflow: Option<String>,
    pub epoch: u64,
    pub cpu_capacity: u64,
    pub tasks: Vec<TaskView>,
    pub pending: Vec<DispatchView>,
}

#[derive(Debug, Serialize)]
pub struct TaskView {
    pub label: String,
    pub task: String,
    pub state: String,
    pub depends_on: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct DispatchView {
    pub label: String,
    pub lease: String,
    pub epoch: u64,
    pub valid: bool,
}

struct Active {
    scheduler: TaskScheduler<JournalWriter>,
    workflow: String,
    epoch: u64,
    order: Vec<(String, TaskId, Vec<String>)>,
    pending: BTreeMap<TaskId, Dispatch>,
}

pub struct KernelSession {
    worker: Arc<Mutex<LocalExecutionRecoveryWorker>>,
    cpu_capacity: u64,
    next_epoch: u64,
    active: Option<Active>,
}

impl KernelSession {
    pub fn open(journal_dir: impl AsRef<std::path::Path>) -> EonResult<Self> {
        let worker = LocalExecutionRecoveryWorker::boot(journal_dir)?;
        let cpu_capacity = std::thread::available_parallelism()
            .map(|n| n.get() as u64)
            .unwrap_or(1);
        Ok(Self {
            worker: Arc::new(Mutex::new(worker)),
            cpu_capacity,
            next_epoch: 1,
            active: None,
        })
    }

    /// Replaces any in-memory workflow. Its journal records stay on disk.
    pub fn submit(&mut self, request: WorkflowRequest) -> EonResult<KernelView> {
        let order = validate(&request, self.cpu_capacity)?;
        let epoch = self.next_epoch;
        let workflow_id = WorkflowId::new();
        let ids: BTreeMap<String, TaskId> = order
            .iter()
            .map(|(label, _)| (label.clone(), TaskId::new()))
            .collect();

        let mut tasks = Vec::with_capacity(order.len());
        for (label, request) in &order {
            let dependencies = request
                .depends_on
                .iter()
                .map(|dep| ids[dep.as_str()].clone())
                .collect::<BTreeSet<_>>();
            let spec = TaskSpec {
                id: ids[label.as_str()].clone(),
                workflow_id: workflow_id.clone(),
                task_type: TaskType::AtomicExecution,
                capability: request.capability.clone(),
                operation: request.operation.clone(),
                parameters: serde_json::json!({}),
                dependencies,
                resource_requirements: vec![Requirement {
                    key: CanonicalResourceKey {
                        rank: ResourceRank::CpuCores,
                        id: String::new(),
                    },
                    demand: Demand::Pool(request.cpu_cores),
                }],
                timeout_ms: request.timeout_ms,
                is_idempotent: request.is_idempotent,
            };
            tasks.push(Task::try_from(spec)?);
        }

        let created_at_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| EonError::ValidationError(e.to_string()))?
            .as_nanos() as u64;
        let workflow_label = workflow_id.to_string();
        let workflow = Workflow::new(workflow_id, ObjectiveId::new(), created_at_ns, epoch, tasks)?;
        let mut resources = ResourceManager::new();
        resources.set_capacity(
            CanonicalResourceKey {
                rank: ResourceRank::CpuCores,
                id: String::new(),
            },
            self.cpu_capacity,
        );
        let sink = JournalWriter(Arc::clone(&self.worker));
        let scheduler = TaskScheduler::new(workflow, epoch, resources, sink)?;

        self.next_epoch += 1;
        let order = order
            .into_iter()
            .map(|(label, request)| {
                let id = ids[label.as_str()].clone();
                (label, id, request.depends_on.clone())
            })
            .collect();
        self.active = Some(Active {
            scheduler,
            workflow: workflow_label,
            epoch,
            order,
            pending: BTreeMap::new(),
        });
        Ok(self.view())
    }

    pub fn tick(&mut self) -> EonResult<KernelView> {
        let active = self.active_mut()?;
        for dispatch in active.scheduler.tick(Instant::now())? {
            active.pending.insert(dispatch.task().clone(), dispatch);
        }
        Ok(self.view())
    }

    pub fn finish(&mut self, label: &str, outcome: Outcome) -> EonResult<KernelView> {
        let active = self.active_mut()?;
        let id = find_id(&active.order, label)?;
        let dispatch = active.pending.remove(&id).ok_or_else(|| {
            EonError::ValidationError(format!("task {label} has no pending dispatch"))
        })?;
        active
            .scheduler
            .finish(&dispatch, outcome, Instant::now())?;
        Ok(self.view())
    }

    /// No compensator is attached to this server, so any completed task that needs undo is reported
    /// as `UndoFailed` rather than silently skipped.
    pub fn countermand(&mut self) -> EonResult<(String, KernelView)> {
        let active = self.active_mut()?;
        let epoch = active.epoch;
        let result = active.scheduler.countermand(epoch, |task| {
            Err(EonError::OperationFailed {
                operation: "countermand".into(),
                reason: format!("no compensator is attached for {}", task.id),
            })
        })?;
        let text = match result {
            Countermand::Compensated => "compensated".to_string(),
            Countermand::NeedsReconciliation(tasks) => {
                format!("needs reconciliation: {} task(s)", tasks.len())
            }
            Countermand::UndoFailed { task, reason } => format!("undo failed for {task}: {reason}"),
        };
        Ok((text, self.view()))
    }

    pub fn view(&self) -> KernelView {
        let Some(active) = &self.active else {
            return KernelView {
                workflow: None,
                epoch: self.next_epoch.saturating_sub(1),
                cpu_capacity: self.cpu_capacity,
                tasks: Vec::new(),
                pending: Vec::new(),
            };
        };
        let tasks = active
            .order
            .iter()
            .map(|(label, id, deps)| TaskView {
                label: label.clone(),
                task: id.to_string(),
                state: active
                    .scheduler
                    .state(id)
                    .map_or_else(|| "unknown".into(), state_name),
                depends_on: deps.clone(),
            })
            .collect();
        let pending = active
            .pending
            .iter()
            .map(|(id, dispatch)| DispatchView {
                label: label_of(&active.order, id),
                lease: dispatch.lease().to_string(),
                epoch: dispatch.epoch(),
                valid: dispatch.is_valid(),
            })
            .collect();
        KernelView {
            workflow: Some(active.workflow.clone()),
            epoch: active.epoch,
            cpu_capacity: self.cpu_capacity,
            tasks,
            pending,
        }
    }

    fn active_mut(&mut self) -> EonResult<&mut Active> {
        self.active
            .as_mut()
            .ok_or_else(|| EonError::ValidationError("no workflow has been submitted".into()))
    }
}

fn validate(
    request: &WorkflowRequest,
    cpu_capacity: u64,
) -> EonResult<Vec<(String, &TaskRequest)>> {
    if request.tasks.is_empty() {
        return Err(EonError::ValidationError("workflow has no tasks".into()));
    }
    if request.tasks.len() > MAX_TASKS {
        return Err(EonError::ValidationError(format!(
            "workflow has {} tasks, the limit is {MAX_TASKS}",
            request.tasks.len()
        )));
    }
    let mut labels = BTreeSet::new();
    for task in &request.tasks {
        let label = task.label.as_str();
        let valid_label = !label.is_empty()
            && label.len() <= MAX_LABEL_BYTES
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if !valid_label {
            return Err(EonError::ValidationError(format!(
                "label {label:?} must be 1 to {MAX_LABEL_BYTES} ASCII letters, digits, '-' or '_'"
            )));
        }
        if !labels.insert(label) {
            return Err(EonError::ValidationError(format!(
                "duplicate label {label}"
            )));
        }
        if task.capability.is_empty() || task.operation.is_empty() {
            return Err(EonError::ValidationError(format!(
                "task {label} needs a capability and an operation"
            )));
        }
        if task.cpu_cores == 0 || task.cpu_cores > cpu_capacity {
            return Err(EonError::ValidationError(format!(
                "task {label} asks for {} cores, this host has {cpu_capacity}",
                task.cpu_cores
            )));
        }
        if task.timeout_ms == 0 || task.timeout_ms > MAX_TIMEOUT_MS {
            return Err(EonError::ValidationError(format!(
                "task {label} timeout must be 1 to {MAX_TIMEOUT_MS} ms"
            )));
        }
    }
    for task in &request.tasks {
        for dep in &task.depends_on {
            if !labels.contains(dep.as_str()) {
                return Err(EonError::ValidationError(format!(
                    "task {} depends on unknown label {dep}",
                    task.label
                )));
            }
        }
    }
    Ok(request
        .tasks
        .iter()
        .map(|task| (task.label.clone(), task))
        .collect())
}

fn find_id(order: &[(String, TaskId, Vec<String>)], label: &str) -> EonResult<TaskId> {
    order
        .iter()
        .find(|(l, _, _)| l == label)
        .map(|(_, id, _)| id.clone())
        .ok_or_else(|| EonError::ValidationError(format!("unknown task label {label}")))
}

fn label_of(order: &[(String, TaskId, Vec<String>)], id: &TaskId) -> String {
    order
        .iter()
        .find(|(_, t, _)| t == id)
        .map(|(l, _, _)| l.clone())
        .unwrap_or_default()
}

fn state_name(state: TaskState) -> String {
    format!("{state:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(label: &str, deps: &[&str]) -> TaskRequest {
        TaskRequest {
            label: label.into(),
            capability: "cap".into(),
            operation: "op".into(),
            depends_on: deps.iter().map(|d| d.to_string()).collect(),
            cpu_cores: 1,
            timeout_ms: 1000,
            is_idempotent: true,
        }
    }

    #[test]
    fn rejects_unknown_dependency_and_duplicate_label() {
        let unknown = WorkflowRequest {
            tasks: vec![task("a", &["missing"])],
        };
        assert!(validate(&unknown, 4).is_err());
        let duplicate = WorkflowRequest {
            tasks: vec![task("a", &[]), task("a", &[])],
        };
        assert!(validate(&duplicate, 4).is_err());
    }

    #[test]
    fn rejects_core_request_above_capacity() {
        let mut big = task("a", &[]);
        big.cpu_cores = 8;
        assert!(validate(&WorkflowRequest { tasks: vec![big] }, 4).is_err());
    }

    #[test]
    fn runs_a_chain_through_the_durable_journal() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = KernelSession::open(dir.path()).unwrap();
        let view = session
            .submit(WorkflowRequest {
                tasks: vec![task("fetch", &[]), task("build", &["fetch"])],
            })
            .unwrap();
        assert_eq!(view.tasks[1].state, "Pending");

        let dispatched = session.tick().unwrap();
        assert_eq!(dispatched.pending.len(), 1);
        assert_eq!(dispatched.pending[0].label, "fetch");

        let after = session.finish("fetch", Outcome::Succeeded).unwrap();
        assert!(after.pending.is_empty());
        let next = session.tick().unwrap();
        assert_eq!(next.pending[0].label, "build");
    }
}
