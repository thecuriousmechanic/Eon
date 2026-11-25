//! Opt-in local observation of an initiator's paired sync.

use crate::{paired, sync::Replica, transport::PeerNode};
use eon_primitives::{EonResult, telemetry::PairedSyncCompleted};
use iroh::EndpointAddr;

#[derive(Debug)]
#[must_use = "check the local journal acknowledgement separately from sync"]
pub struct CompletedSync {
    /// An error means recording was not confirmed, **not** that sync should be retried.
    pub journal_seq: EonResult<u64>,
}

/// Use instead of [`paired::sync`] only when the operator opts in. The observer
/// runs once, locally, after success. Ordinary sync and responders log nothing.
///
/// The trusted runtime supplies its existing paired node and a sink for a local
/// telemetry journal (not the kernel's event-only recovery journal). The marker
/// identifies a record type, not its author: use a journal controlled by trusted
/// local producers. Open `/mesh` in `eon watch <journal-dir>`; watch never dials.
///
/// ```no_run
/// # use eon_foundation::group_commit::GroupCommitJournal;
/// # use eon_mesh::{EonResult, observed::sync_with_observer, sync::Replica, transport::PeerNode};
/// # async fn opted_in(node: &PeerNode, peer: iroh::EndpointAddr, replica: &Replica, journal: &GroupCommitJournal) -> EonResult<()> {
/// let completed = sync_with_observer(node, peer, replica, |marker| {
///     journal.append_durable(marker.encode().to_vec())
/// }).await?;
/// if completed.journal_seq.is_err() {
///     eprintln!("Sync completed; local telemetry was not confirmed.");
/// }
/// # Ok(()) }
/// ```
pub async fn sync_with_observer(
    node: &PeerNode,
    peer: EndpointAddr,
    replica: &Replica,
    record: impl FnOnce(PairedSyncCompleted) -> EonResult<u64>,
) -> EonResult<CompletedSync> {
    paired::sync(node, peer, replica).await?;
    Ok(CompletedSync {
        journal_seq: record(PairedSyncCompleted),
    })
}
