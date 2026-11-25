//! Immutable submission contracts. Execution progress is not part of a submitted DAG.

pub mod objective;
pub mod task;
pub mod workflow;

pub use objective::{Objective, ObjectiveSpec, Priority};
pub use task::{Task, TaskSpec, TaskType};
pub use workflow::Workflow;
