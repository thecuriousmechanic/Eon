//! Replicated projection state over Loro CRDTs (docs/implementation/05, section 3).
//!
//! Each device holds a replica of the shared projection. Replicas accept local edits offline, exchange update
//! bytes, and converge without coordination. Imports are idempotent, so a duplicate or replayed update is harmless.
//! The public string map and paired projections have disjoint documents, histories and frontiers.

#[cfg(test)]
mod privacy_tests;
mod pages;
pub(crate) use pages::{Page, check_writer_count};
mod projections;
pub use projections::{ContextEntry, ContextRecord, Entity, KnowledgeGraph, Relation, Speaker, WorkingContext};

use loro::{
    ContainerID, ContainerType, EncodedBlobMode, ExportMode, JsonMapOp, JsonOpContent,
    LoroDoc, LoroValue, VersionVector,
};

use eon_primitives::{EonError, EonResult};

const PROJECTION: &str = "projection";

/// Local protected writes (UTF-8 key plus postcard-encoded value) reserve
/// half a frame for Loro metadata and paired framing. This admission limit is
/// not retroactive: larger legacy records can still travel in a fitting page.
pub const MAX_PROTECTED_RECORD_BYTES: usize = crate::transport::MAX_FRAME_BYTES / 2;

pub struct Replica {
    public: LoroDoc,
    protected: LoroDoc,
}

impl Replica {
    /// `peer_id` identifies a Loro writer, not a network credential. Never reuse
    /// the same writer ID for independent histories, including after state loss.
    pub fn new(peer_id: u64) -> EonResult<Self> {
        let public = LoroDoc::new();
        let protected = LoroDoc::new();
        public.set_peer_id(peer_id).map_err(loro_err)?;
        protected.set_peer_id(peer_id).map_err(loro_err)?;
        Ok(Self { public, protected })
    }

    /// Writes public data only. This legacy map is available to unpaired peers.
    pub fn set(&self, key: &str, value: &str) -> EonResult<()> {
        self.public.get_map(PROJECTION).insert(key, value).map_err(loro_err)?;
        self.public.commit();
        Ok(())
    }

    pub fn get(&self, key: &str) -> Option<String> {
        match self.public.get_map(PROJECTION).get(key)? {
            loro::ValueOrContainer::Value(LoroValue::String(s)) => Some(s.to_string()),
            _ => None,
        }
    }

    /// Public history only. No protected operations or frontiers are included.
    pub fn export_all(&self) -> EonResult<Vec<u8>> {
        self.public.export(ExportMode::all_updates()).map_err(loro_err)
    }

    /// This replica's version vector. A peer sends it back so this replica can export only what the peer lacks.
    pub fn version(&self) -> Vec<u8> {
        self.public.oplog_vv().encode()
    }

    /// The updates a peer lacks, given the version vector it sent. Empty of new operations when the peer is current.
    pub fn export_since(&self, peer_version: &[u8]) -> EonResult<Vec<u8>> {
        export_since(&self.public, peer_version)
    }

    /// Accepts only complete histories/deltas of public string-map operations.
    pub fn apply(&self, update: &[u8]) -> EonResult<()> {
        import_checked(&self.public, update, |root, op| {
            root == PROJECTION
                && matches!(
                    op,
                    JsonMapOp::Delete { .. }
                        | JsonMapOp::Insert {
                            value: LoroValue::String(_),
                            ..
                        }
                )
        })
    }

    pub(crate) fn paired_version(&self) -> VersionVector {
        self.protected.oplog_vv()
    }

    #[cfg(test)]
    pub(crate) fn paired_export_since(&self, version: &[u8]) -> EonResult<Vec<u8>> {
        export_since(&self.protected, version)
    }

    #[cfg(test)]
    pub(crate) fn paired_apply(&self, update: &[u8]) -> EonResult<()> {
        import_checked(&self.protected, update, projections::allowed)
    }
}

fn export_since(doc: &LoroDoc, version: &[u8]) -> EonResult<Vec<u8>> {
    let from = VersionVector::decode(version).map_err(loro_err)?;
    doc.export(ExportMode::updates(&from)).map_err(loro_err)
}

fn rejected() -> EonError {
    EonError::ValidationError("mesh import rejected: invalid or out-of-domain history".into())
}

// Inspect *operations*, not the winning values: overwritten/deleted records, commit
// messages and pending operations must not hitch a ride on a later export. Only
// reconstruct the validated operations in the live document; never import opaque
// snapshot state, dictionaries or unchecked bytes there.
fn import_checked(doc: &LoroDoc, bytes: &[u8], allowed: impl Fn(&str, &JsonMapOp) -> bool) -> EonResult<()> {
    let meta = LoroDoc::decode_import_blob_meta(bytes, true).map_err(|_| rejected())?;
    if !matches!(meta.mode, EncodedBlobMode::Updates | EncodedBlobMode::Snapshot) {
        return Err(rejected());
    }
    let scratch = doc.fork();
    let before = scratch.oplog_vv();
    if scratch.import(bytes).map_err(|_| rejected())?.pending.is_some() {
        return Err(rejected());
    }
    let after = scratch.oplog_vv();
    let ops = scratch.export_json_updates(&before, &after).map_err(|_| rejected())?;
    for change in &ops.changes {
        if change.msg.is_some() {
            return Err(rejected());
        }
        for op in &change.ops {
            let (
                ContainerID::Root {
                    name,
                    container_type: ContainerType::Map,
                },
                JsonOpContent::Map(write),
            ) = (&op.container, &op.content)
            else {
                return Err(rejected());
            };
            if !allowed(name.as_str(), write) {
                return Err(rejected());
            }
        }
    }
    if !ops.changes.is_empty() && doc.import_json_updates(ops).map_err(|_| rejected())?.pending.is_some() {
        return Err(rejected());
    }
    if !doc.oplog_vv().includes_vv(&after) {
        return Err(rejected());
    }
    Ok(())
}

fn loro_err(e: impl std::fmt::Display) -> EonError {
    EonError::InternalError(format!("loro: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_way_sync_delivers_writes() {
        let a = Replica::new(1).unwrap();
        let b = Replica::new(2).unwrap();
        a.set("device", "phone").unwrap();
        b.apply(&a.export_all().unwrap()).unwrap();
        assert_eq!(b.get("device").as_deref(), Some("phone"));
    }

    #[test]
    fn concurrent_offline_edits_converge_on_both_replicas() {
        let a = Replica::new(1).unwrap();
        let b = Replica::new(2).unwrap();
        a.set("owner_note", "from laptop").unwrap();
        b.set("owner_note", "from phone").unwrap();
        b.set("battery", "80").unwrap();

        a.apply(&b.export_all().unwrap()).unwrap();
        b.apply(&a.export_all().unwrap()).unwrap();

        assert_eq!(
            a.get("owner_note"),
            b.get("owner_note"),
            "replicas must agree on the conflicting key"
        );
        assert!(a.get("owner_note").is_some());
        assert_eq!(a.get("battery").as_deref(), Some("80"));
        assert_eq!(b.get("battery").as_deref(), Some("80"));
    }

    #[test]
    fn replayed_updates_are_idempotent() {
        let a = Replica::new(1).unwrap();
        let b = Replica::new(2).unwrap();
        a.set("k", "v").unwrap();
        let update = a.export_all().unwrap();
        b.apply(&update).unwrap();
        let once = b.export_all().unwrap();
        b.apply(&update).unwrap();
        b.apply(&update).unwrap();
        assert_eq!(b.get("k").as_deref(), Some("v"));
        assert_eq!(b.export_all().unwrap(), once);
    }

    #[test]
    fn corrupt_update_is_refused() {
        let b = Replica::new(2).unwrap();
        assert!(b.apply(b"not a loro update").is_err());
    }

    #[test]
    fn delta_carries_only_what_the_peer_lacks() {
        let a = Replica::new(1).unwrap();
        let b = Replica::new(2).unwrap();
        for i in 0..100 {
            a.set(&format!("old{i}"), "x").unwrap();
        }
        b.apply(&a.export_all().unwrap()).unwrap();

        a.set("new", "y").unwrap();
        let delta = a.export_since(&b.version()).unwrap();
        assert!(
            delta.len() < a.export_all().unwrap().len() / 4,
            "delta must not repeat the shared history"
        );

        b.apply(&delta).unwrap();
        assert_eq!(b.get("new").as_deref(), Some("y"));
        assert_eq!(b.get("old99").as_deref(), Some("x"));
    }

    #[test]
    fn corrupt_version_vector_is_refused() {
        let a = Replica::new(1).unwrap();
        assert!(a.export_since(b"not a version").is_err());
    }

    #[test]
    fn three_replicas_converge_in_any_order() {
        let replicas: Vec<Replica> = (1..=3).map(|i| Replica::new(i).unwrap()).collect();
        replicas[0].set("a", "1").unwrap();
        replicas[1].set("b", "2").unwrap();
        replicas[2].set("c", "3").unwrap();
        let updates: Vec<Vec<u8>> = replicas.iter().map(|r| r.export_all().unwrap()).collect();
        for order in [[0usize, 1, 2], [2, 0, 1], [1, 2, 0]] {
            let fresh: Vec<Replica> = (10..13).map(|i| Replica::new(i).unwrap()).collect();
            for &i in &order {
                for r in &fresh {
                    r.apply(&updates[i]).unwrap();
                }
            }
            for r in &fresh {
                assert_eq!(r.get("a").as_deref(), Some("1"));
                assert_eq!(r.get("b").as_deref(), Some("2"));
                assert_eq!(r.get("c").as_deref(), Some("3"));
            }
        }
    }
}
