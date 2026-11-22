use std::collections::{BTreeMap, BTreeSet};

use eon_primitives::{TaskId, dag::topological_order};

use crate::{BridgeError, invalid};

pub struct DagNode {
    pub task_id: String,
    pub dependencies: Vec<String>,
}

const MAX_NODES: usize = 256;
const MAX_EDGES: usize = 4096;

/// Validate only the dependency projection, not a workflow submission or its capabilities.
pub fn validate_task_dag(tasks: Vec<DagNode>) -> Result<Vec<String>, BridgeError> {
    if tasks.len() > MAX_NODES {
        return Err(invalid("too many DAG nodes"));
    }
    let task_id = |s| TaskId::from_raw(s).map_err(|_| invalid("invalid DAG task ID"));
    let mut graph = BTreeMap::new();
    let mut edge_budget = MAX_EDGES;
    for task in tasks {
        let id = task_id(task.task_id)?;
        edge_budget = edge_budget
            .checked_sub(task.dependencies.len())
            .ok_or_else(|| invalid("too many DAG dependencies"))?;
        let dependencies = task
            .dependencies
            .into_iter()
            .map(task_id)
            .collect::<Result<BTreeSet<_>, _>>()?;
        if graph.insert(id, dependencies).is_some() {
            return Err(invalid("duplicate DAG task"));
        }
    }
    let order = topological_order(&graph, |deps| deps).map_err(|e| invalid(e.to_string()))?;
    Ok(order.iter().map(ToString::to_string).collect())
}

#[cfg(test)]
mod codegen_tests {
    use super::*;

    fn node(id: &TaskId, deps: &[&TaskId]) -> DagNode {
        DagNode {
            task_id: id.to_string(),
            dependencies: deps.iter().map(|d| d.to_string()).collect(),
        }
    }

    #[test]
    fn stable_order_accepts_forward_references_and_set_valued_dependencies() {
        let [sink, a, b, root] = std::array::from_fn(|_| TaskId::new());
        let tasks = vec![
            node(&sink, &[&b, &a]),
            node(&b, &[&root]),
            node(&a, &[&root, &root]),
            node(&root, &[]),
        ];
        assert_eq!(
            validate_task_dag(tasks).unwrap(),
            [root, a, b, sink].map(|id| id.to_string())
        );
        assert!(validate_task_dag(vec![]).unwrap().is_empty());
    }

    #[test]
    fn rejects_invalid_ids_missing_nodes_duplicates_cycles_and_oversized_graphs() {
        let [a, b, root] = std::array::from_fn(|_| TaskId::new());
        for tasks in [
            vec![DagNode {
                task_id: crate::WorkflowId::new().to_string(),
                dependencies: vec![],
            }],
            vec![DagNode {
                task_id: a.to_string(),
                dependencies: vec!["task_bad".into()],
            }],
            vec![node(&a, &[&b])],
            vec![node(&a, &[]), node(&a, &[])],
            vec![node(&a, &[&a])],
            vec![node(&root, &[]), node(&a, &[&b]), node(&b, &[&a])],
            (0..=MAX_NODES).map(|_| node(&TaskId::new(), &[])).collect(),
            vec![node(&a, &vec![&b; MAX_EDGES + 1]), node(&b, &[])],
        ] {
            assert!(matches!(
                validate_task_dag(tasks),
                Err(BridgeError::InvalidInput { .. })
            ));
        }
    }
}
