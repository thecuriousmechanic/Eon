//! EON Execution Kernel: DAG scheduling, resource leases, task state machine, and saga rollback.
pub mod continuation;
pub mod dispatcher;
#[cfg(feature = "host")]
pub mod epoch;
pub mod events;
pub mod fsm;
#[cfg(feature = "host")]
pub mod journal_sink;
pub mod model;
#[cfg(feature = "host")]
pub mod overlay;
#[cfg(feature = "host")]
pub mod recovery;
pub mod resource;
pub mod saga;
#[cfg(feature = "host")]
pub mod schedule;
pub mod scheduler;
#[cfg(feature = "host")]
pub mod shadow;
#[cfg(feature = "host")]
pub mod structural;
pub mod workflow;
#[cfg(feature = "host")]
pub mod worktree;

pub use eon_primitives::*;
