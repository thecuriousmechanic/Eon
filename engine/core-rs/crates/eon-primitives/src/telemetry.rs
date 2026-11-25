//! Local journal markers, not mesh wire messages.

/// One completed call to paired sync. The marker has no fields: the journal
/// supplies its sequence number, and no data from the peer is recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PairedSyncCompleted;

impl PairedSyncCompleted {
    const V1: &'static [u8] = b"EON\0mesh.paired-sync.completed\x01";

    pub fn encode(self) -> &'static [u8] {
        Self::V1
    }

    pub fn decode(payload: &[u8]) -> Option<Self> {
        (payload == Self::V1).then_some(Self)
    }
}
