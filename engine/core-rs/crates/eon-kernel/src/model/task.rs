use std::{collections::BTreeSet, ops::Deref};

use eon_primitives::{EonError, TaskId, WorkflowId};
use serde::{Deserialize, Serialize, Serializer};

use crate::resource::{Requirement, merge};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskType {
    AtomicExecution,
    CognitiveContinuation,
    SubAgentSpawn {
        sub_agent_role: String,
        prompt: String,
    },
}

/// Unchecked construction input. A [`Task`] exposes these fields read-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSpec {
    pub id: TaskId,
    pub workflow_id: WorkflowId,
    pub task_type: TaskType,
    pub capability: String,
    pub operation: String,
    /// Schema validation for this payload belongs to the target capability.
    pub parameters: serde_json::Value,
    pub dependencies: BTreeSet<TaskId>,
    pub resource_requirements: Vec<Requirement>,
    pub timeout_ms: u64,
    pub is_idempotent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "TaskSpec")]
pub struct Task(TaskSpec);

impl Serialize for Task {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl TryFrom<TaskSpec> for Task {
    type Error = EonError;

    fn try_from(mut spec: TaskSpec) -> Result<Self, Self::Error> {
        if spec.capability.trim().is_empty() || spec.operation.trim().is_empty() {
            return Err(EonError::ValidationError(
                "task needs a capability and operation".into(),
            ));
        }
        if spec.timeout_ms == 0 {
            return Err(EonError::ValidationError(
                "task timeout must be positive".into(),
            ));
        }
        if let TaskType::SubAgentSpawn {
            sub_agent_role,
            prompt,
        } = &spec.task_type
            && (sub_agent_role.trim().is_empty() || prompt.trim().is_empty())
        {
            return Err(EonError::ValidationError(
                "sub-agent needs a role and prompt".into(),
            ));
        }
        if spec.dependencies.contains(&spec.id) {
            return Err(EonError::ValidationError(format!(
                "task {} depends on itself",
                spec.id
            )));
        }
        spec.parameters.sort_all_objects();
        spec.resource_requirements = merge(spec.resource_requirements)?;
        Ok(Self(spec))
    }
}

impl Deref for Task {
    type Target = TaskSpec;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
