//! Read-only checks. Evidence describes the supplied artifact or snapshot, not an
//! application's security or the cause of an event. No check applies a repair.

pub mod auto_patch;
pub mod taint_auditor;
pub mod telemetry;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Assessment<T> {
    Observed(T),
    Unverified(Gap),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gap {
    Unsupported,
    Unavailable,
    OutsideRule,
    MissingEvidence,
    InvalidInput,
    Ambiguous,
    LimitExceeded,
}

pub const MAX_SOURCE_BYTES: usize = 16 * 1024;
