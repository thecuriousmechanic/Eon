//! Workflow DAG with Kahn validation and dynamic subgraph splicing (docs/implementation/01, section 4.2).

use std::collections::{BTreeMap, BTreeSet};

use eon_primitives::{EonError, EonResult, TaskId, dag::topological_order};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Pending,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: TaskId,
    pub dependencies: BTreeSet<TaskId>,
    pub state: TaskState,
}

impl Task {
    pub fn new(id: TaskId, dependencies: impl IntoIterator<Item = TaskId>) -> Self {
        Self {
            id,
            dependencies: dependencies.into_iter().collect(),
            state: TaskState::Pending,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Workflow {
    tasks: BTreeMap<TaskId, Task>,
}

/// Project the validated definition into a fresh execution graph. All tasks start
/// pending; capability contracts and objective metadata stay in the immutable definition.
impl From<&crate::model::Workflow> for Workflow {
    fn from(definition: &crate::model::Workflow) -> Self {
        let tasks = definition
            .tasks()
            .iter()
            .map(|(id, t)| {
                (
                    id.clone(),
                    Task::new(id.clone(), t.dependencies.iter().cloned()),
                )
            })
            .collect();
        Self { tasks }
    }
}

impl Workflow {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, task: Task) -> EonResult<()> {
        if self.tasks.contains_key(&task.id) {
            return Err(EonError::ValidationError(format!(
                "duplicate task {}",
                task.id
            )));
        }
        let id = task.id.clone();
        self.tasks.insert(id.clone(), task);
        if let Err(err) = self.validate_topological_order() {
            self.tasks.remove(&id);
            return Err(err);
        }
        Ok(())
    }

    pub fn get(&self, id: &TaskId) -> Option<&Task> {
        self.tasks.get(id)
    }

    pub fn complete(&mut self, id: &TaskId) -> EonResult<()> {
        let task = self
            .tasks
            .get_mut(id)
            .ok_or_else(|| EonError::ValidationError(format!("unknown task {id}")))?;
        task.state = TaskState::Completed;
        Ok(())
    }

    /// Pending tasks whose dependencies are all completed, in stable id order.
    pub fn ready(&self) -> Vec<TaskId> {
        self.tasks
            .values()
            .filter(|t| t.state == TaskState::Pending)
            .filter(|t| {
                t.dependencies.iter().all(|d| {
                    self.tasks
                        .get(d)
                        .is_some_and(|dep| dep.state == TaskState::Completed)
                })
            })
            .map(|t| t.id.clone())
            .collect()
    }

    /// Kahn's algorithm: every dependency must exist and the graph must have no cycle.
    pub fn validate_topological_order(&self) -> EonResult<()> {
        topological_order(&self.tasks, |t| &t.dependencies).map(|_| ())
    }

    /// Replaces a completed continuation `parent` with `children`. Downstream tasks that depended
    /// on `parent` now depend on the children's sink nodes. The whole change is reverted on any error.
    pub fn splice_subgraph(&mut self, parent: &TaskId, children: Vec<Task>) -> EonResult<()> {
        let parent_task = self
            .tasks
            .get(parent)
            .ok_or_else(|| EonError::ValidationError(format!("unknown parent {parent}")))?;
        if parent_task.state != TaskState::Completed {
            return Err(EonError::ValidationError(format!(
                "parent {parent} must be completed before splicing"
            )));
        }
        if children.is_empty() {
            return Err(EonError::ValidationError(
                "splice requires at least one child task".to_string(),
            ));
        }

        let child_ids: BTreeSet<TaskId> = children.iter().map(|c| c.id.clone()).collect();
        if child_ids.len() != children.len()
            || child_ids.iter().any(|id| self.tasks.contains_key(id))
        {
            return Err(EonError::ValidationError(
                "child task ids must be fresh and unique".to_string(),
            ));
        }
        for child in &children {
            for dep in &child.dependencies {
                let known_completed = self
                    .tasks
                    .get(dep)
                    .is_some_and(|t| t.state == TaskState::Completed);
                if !child_ids.contains(dep) && !known_completed {
                    return Err(EonError::ValidationError(format!(
                        "child {} depends on {dep}, which is neither a completed task nor a sibling",
                        child.id
                    )));
                }
            }
        }

        let referenced_by_siblings: BTreeSet<&TaskId> = children
            .iter()
            .flat_map(|c| c.dependencies.iter())
            .collect();
        let sinks: Vec<TaskId> = child_ids
            .iter()
            .filter(|id| !referenced_by_siblings.contains(id))
            .cloned()
            .collect();

        let snapshot = self.clone();
        for task in self.tasks.values_mut() {
            if task.id != *parent && task.dependencies.remove(parent) {
                task.dependencies.extend(sinks.iter().cloned());
            }
        }
        for mut child in children {
            if child.dependencies.is_empty() {
                child.dependencies.insert(parent.clone());
            }
            self.tasks.insert(child.id.clone(), child);
        }

        if let Err(e) = self.validate_topological_order() {
            *self = snapshot;
            return Err(e);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> TaskId {
        TaskId::from_raw(format!("task_{}", zero_pad(s))).unwrap()
    }

    fn zero_pad(s: &str) -> String {
        format!("{:0>26}", s.to_uppercase())
    }

    fn base_workflow() -> (Workflow, TaskId, TaskId) {
        let mut wf = Workflow::new();
        let parent = id("1");
        let downstream = id("2");
        wf.insert(Task::new(parent.clone(), [])).unwrap();
        wf.insert(Task::new(downstream.clone(), [parent.clone()]))
            .unwrap();
        wf.complete(&parent).unwrap();
        (wf, parent, downstream)
    }

    #[test]
    fn ready_respects_dependencies() {
        let (wf, parent, downstream) = base_workflow();
        assert_eq!(wf.ready(), vec![downstream]);
        assert!(!wf.ready().contains(&parent));
    }

    #[test]
    fn splice_rewires_downstream_to_sink_and_roots_to_parent() {
        let (mut wf, parent, downstream) = base_workflow();
        let root = id("10");
        let sink = id("11");
        let children = vec![
            Task::new(root.clone(), []),
            Task::new(sink.clone(), [root.clone()]),
        ];
        wf.splice_subgraph(&parent, children).unwrap();

        assert!(wf.get(&root).unwrap().dependencies.contains(&parent));
        let downstream_deps = &wf.get(&downstream).unwrap().dependencies;
        assert!(
            downstream_deps.contains(&sink),
            "downstream must wait for the sink"
        );
        assert!(!downstream_deps.contains(&parent));
        assert!(
            !wf.ready().contains(&downstream),
            "downstream stays blocked until the sink completes"
        );
    }

    #[test]
    fn splice_that_creates_cycle_is_reverted() {
        let (mut wf, parent, downstream) = base_workflow();
        let before = wf.clone();
        let a = id("20");
        let b = id("21");
        let children = vec![
            Task::new(a.clone(), [b.clone()]),
            Task::new(b.clone(), [a.clone()]),
        ];
        assert!(wf.splice_subgraph(&parent, children).is_err());
        assert!(wf.get(&a).is_none());
        assert_eq!(
            wf.get(&downstream).unwrap().dependencies,
            before.get(&downstream).unwrap().dependencies
        );
    }

    #[test]
    fn splice_rejects_pending_parent_and_reused_ids() {
        let mut wf = Workflow::new();
        let pending = id("30");
        wf.insert(Task::new(pending.clone(), [])).unwrap();
        assert!(
            wf.splice_subgraph(&pending, vec![Task::new(id("31"), [])])
                .is_err()
        );

        wf.complete(&pending).unwrap();
        assert!(
            wf.splice_subgraph(&pending, vec![Task::new(pending.clone(), [])])
                .is_err()
        );
    }

    #[test]
    fn rejected_insert_leaves_existing_tasks_untouched() {
        let mut wf = Workflow::new();
        wf.insert(Task::new(id("1"), [])).unwrap();
        let before = wf.clone();
        assert!(wf.insert(Task::new(id("40"), [id("99")])).is_err());
        assert_eq!(wf, before);
        assert!(wf.insert(Task::new(id("40"), [id("40")])).is_err());
        assert_eq!(wf, before);
    }
}
