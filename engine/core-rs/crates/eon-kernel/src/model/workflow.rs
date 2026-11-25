use std::collections::BTreeMap;

use eon_primitives::{
    EonError, EonResult, ObjectiveId, TaskId, WorkflowId, dag::topological_order,
};
use serde::{Deserialize, Serialize, Serializer};

use super::Task;

/// Identity of the complete validated definition (including normalized requirements,
/// sorted task IDs, dependencies and parameter object keys); never log the serialized bytes.
pub fn workflow_digest(workflow: &Workflow) -> EonResult<[u8; 32]> {
    let mut hasher = blake3::Hasher::new_derive_key("eon.kernel.workflow.v1");
    serde_json::to_writer(&mut hasher, workflow)
        .map_err(|e| EonError::ValidationError(format!("cannot identify workflow: {e}")))?;
    Ok(*hasher.finalize().as_bytes())
}

/// A validated submission. On the wire, tasks are a sequence in ID order:
/// each node declares its own ID, and duplicate nodes cannot be lost to map decoding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "WorkflowSpec")]
pub struct Workflow {
    id: WorkflowId,
    objective_id: ObjectiveId,
    created_at_ns: u64,
    epoch: u64,
    #[serde(serialize_with = "serialize_tasks")]
    tasks: BTreeMap<TaskId, Task>,
}

fn serialize_tasks<S: Serializer>(
    tasks: &BTreeMap<TaskId, Task>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_seq(tasks.values())
}

#[derive(Deserialize)]
struct WorkflowSpec {
    id: WorkflowId,
    objective_id: ObjectiveId,
    created_at_ns: u64,
    epoch: u64,
    tasks: Vec<Task>,
}

impl TryFrom<WorkflowSpec> for Workflow {
    type Error = EonError;

    fn try_from(spec: WorkflowSpec) -> EonResult<Self> {
        Self::new(
            spec.id,
            spec.objective_id,
            spec.created_at_ns,
            spec.epoch,
            spec.tasks,
        )
    }
}

impl Workflow {
    /// Accepts tasks in any order, including forward references. `created_at_ns`
    /// uses the submitting objective's monotonic clock origin.
    pub fn new(
        id: WorkflowId,
        objective_id: ObjectiveId,
        created_at_ns: u64,
        epoch: u64,
        tasks: impl IntoIterator<Item = Task>,
    ) -> EonResult<Self> {
        let mut by_id = BTreeMap::new();
        for task in tasks {
            if task.workflow_id != id {
                return Err(EonError::ValidationError(format!(
                    "task {} belongs to workflow {}, not {id}",
                    task.id, task.workflow_id
                )));
            }
            if let Some(duplicate) = by_id.insert(task.id.clone(), task) {
                return Err(EonError::ValidationError(format!(
                    "duplicate task {}",
                    duplicate.id
                )));
            }
        }
        let workflow = Self {
            id,
            objective_id,
            created_at_ns,
            epoch,
            tasks: by_id,
        };
        workflow.validate_topological_order()?;
        Ok(workflow)
    }

    pub fn id(&self) -> &WorkflowId {
        &self.id
    }

    pub fn objective_id(&self) -> &ObjectiveId {
        &self.objective_id
    }

    pub fn created_at_ns(&self) -> u64 {
        self.created_at_ns
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn tasks(&self) -> &BTreeMap<TaskId, Task> {
        &self.tasks
    }

    /// Returns every task ID once, after all its upstream dependencies.
    pub fn validate_topological_order(&self) -> EonResult<Vec<TaskId>> {
        topological_order(&self.tasks, |t| &t.dependencies)
    }
}
