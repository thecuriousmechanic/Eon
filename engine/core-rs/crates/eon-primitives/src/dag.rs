use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::{EonError, EonResult, TaskId};

/// Validate a task graph and return its dependency-first order. Roots and siblings
/// enter the queue in stable ID order; each outgoing edge is visited once.
pub fn topological_order<T>(
    tasks: &BTreeMap<TaskId, T>,
    dependencies: impl Fn(&T) -> &BTreeSet<TaskId>,
) -> EonResult<Vec<TaskId>> {
    let mut degree = BTreeMap::new();
    let mut dependents: BTreeMap<&TaskId, Vec<&TaskId>> = BTreeMap::new();
    let mut ready = VecDeque::new();
    for (id, task) in tasks {
        let deps = dependencies(task);
        degree.insert(id, deps.len());
        if deps.is_empty() {
            ready.push_back(id);
        }
        for dep in deps {
            if !tasks.contains_key(dep) {
                return Err(EonError::ValidationError(format!(
                    "task {id} depends on unknown task {dep}"
                )));
            }
            dependents.entry(dep).or_default().push(id);
        }
    }
    let mut order = Vec::with_capacity(tasks.len());
    while let Some(id) = ready.pop_front() {
        order.push(id.clone());
        if let Some(children) = dependents.get(id) {
            for child in children {
                let remaining = degree.get_mut(child).expect("every task is indexed");
                *remaining -= 1;
                if *remaining == 0 {
                    ready.push_back(child);
                }
            }
        }
    }
    if order.len() != tasks.len() {
        return Err(EonError::ValidationError(
            "workflow contains a dependency cycle".into(),
        ));
    }
    Ok(order)
}
