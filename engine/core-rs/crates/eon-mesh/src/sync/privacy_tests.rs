use super::*;
use crate::exchange::{answer, offer};

const CLIP: &str = "synthetic-clipboard-canary-93";
const TURN: &str = "synthetic-utterance-canary-46";

#[test]
fn privacy_rejects_uninspectable_or_pending_history_instead_of_buffering_it() {
    let b = Replica::new(2).unwrap();
    let old = LoroDoc::new();
    old.get_map(PROJECTION).insert("ok", "public").unwrap();
    old.commit();
    let base = old.oplog_vv();
    let safe = old.export(ExportMode::all_updates()).unwrap();
    old.get_map("context.entries.v1").insert("clip", CLIP).unwrap();
    old.commit();
    assert!(b.apply(&old.export(ExportMode::updates(&base)).unwrap()).is_err());
    b.apply(&safe).unwrap();
    let before = b.export_all().unwrap();
    let shallow = old
        .export(ExportMode::shallow_snapshot(&old.oplog_frontiers()))
        .unwrap();
    assert!(b.apply(&shallow).is_err());
    assert!(b.paired_apply(&shallow).is_err());

    let with_message = LoroDoc::new();
    with_message.get_map(PROJECTION).insert("ok", "public").unwrap();
    with_message.set_next_commit_message(TURN);
    with_message.commit();
    assert!(
        b.apply(&with_message.export(ExportMode::all_updates()).unwrap())
            .is_err()
    );
    assert_eq!(b.export_all().unwrap(), before);
    // The safe dependency did not release a previously buffered protected operation.
    let decoded = LoroDoc::new();
    decoded.import(&before).unwrap();
    assert_eq!(decoded.oplog_vv(), base);
}

#[test]
fn privacy_serialized_history_contains_only_context_metadata_even_before_deletion() {
    let a = Replica::new(1).unwrap();
    a.set("status", "online").unwrap();
    let public = a.export_all().unwrap();
    let public_version = a.version();
    let public_offer = offer(&a).unwrap();
    a.put_context("clip", &ContextEntry::Clipboard { text: CLIP.into() })
        .unwrap();
    a.put_context(
        "turn",
        &ContextEntry::Utterance {
            speaker: Speaker::User,
            text: TURN.into(),
        },
    )
    .unwrap();
    let initial = a.paired_export_since(&VersionVector::default().encode()).unwrap();
    a.remove_context("turn").unwrap();
    a.remove_context("clip").unwrap();
    let protected = a.paired_export_since(&VersionVector::default().encode()).unwrap();
    assert_eq!(a.version(), public_version);
    assert_eq!(a.export_all().unwrap(), public);
    assert_eq!(offer(&a).unwrap(), public_offer);

    // Raw byte searches alone can miss compressed or tombstoned data. Decode the
    // actual outbound bytes and check every historical write, not just live state.
    for blob in [&initial, &protected, &a.protected.export(ExportMode::Snapshot).unwrap()] {
        let decoded = LoroDoc::new();
        decoded.import(blob).unwrap();
        let log = decoded
            .export_json_updates(&VersionVector::default(), &decoded.oplog_vv())
            .unwrap();
        let mut inserts = 0;
        for change in log.changes {
            assert!(change.msg.is_none());
            for op in change.ops {
                assert_eq!(
                    op.container,
                    ContainerID::new_root("context.entries.v2", ContainerType::Map)
                );
                match op.content {
                    JsonOpContent::Map(JsonMapOp::Insert { key, value }) => {
                        let record = match key.as_str() {
                            "clip" => ContextRecord::Clipboard,
                            "turn" => ContextRecord::Utterance { speaker: Speaker::User },
                            _ => panic!("unexpected key"),
                        };
                        assert_eq!(value, LoroValue::from(postcard::to_allocvec(&record).unwrap()));
                        inserts += 1;
                    }
                    JsonOpContent::Map(JsonMapOp::Delete { key }) => assert!(key == "clip" || key == "turn"),
                    _ => panic!("unexpected operation"),
                }
            }
        }
        assert_eq!(inserts, 2);
    }

    let b = Replica::new(2).unwrap();
    assert!(b.apply(&protected).is_err());
    b.paired_apply(&initial).unwrap();
    assert_eq!(b.working_context().unwrap()["clip"], ContextRecord::Clipboard);
    b.paired_apply(&protected).unwrap();
    b.paired_apply(&initial).unwrap();
    assert!(b.working_context().unwrap().is_empty());
}

#[test]
fn privacy_refuses_mixed_old_snapshots_and_tombstoned_updates_without_partial_import() {
    let legacy = LoroDoc::new();
    legacy.set_peer_id(9).unwrap();
    legacy.get_map(PROJECTION).insert("ok", "legacy").unwrap();
    legacy.commit();
    let safe_snapshot = legacy.export(ExportMode::Snapshot).unwrap();
    let public_only = Replica::new(5).unwrap();
    public_only.apply(&safe_snapshot).unwrap();
    assert_eq!(public_only.get("ok").as_deref(), Some("legacy"));
    assert!(public_only.paired_apply(&safe_snapshot).is_err());

    // v1 serialized an utterance as (variant 0, speaker, text).
    let raw = postcard::to_allocvec(&(0_u8, Speaker::User, TURN)).unwrap();
    let map = legacy.get_map("context.entries.v1");
    map.insert("turn", raw.clone()).unwrap();
    legacy.commit();
    map.delete("turn").unwrap();
    legacy.commit();
    assert!(map.is_empty());
    let mixed = legacy.export(ExportMode::Snapshot).unwrap();
    let updates = legacy.export(ExportMode::all_updates()).unwrap();

    // A v2 root name cannot bless v1 payload bytes, even if they were overwritten.
    let forged = LoroDoc::new();
    let map = forged.get_map("context.entries.v2");
    map.insert("turn", raw).unwrap();
    forged.commit();
    map.insert("turn", postcard::to_allocvec(&ContextRecord::Clipboard).unwrap())
        .unwrap();
    forged.commit();

    let b = Replica::new(2).unwrap();
    b.set("keep", "public").unwrap();
    b.put_context("keep", &ContextEntry::Clipboard { text: CLIP.into() })
        .unwrap();
    let public_before = b.export_all().unwrap();
    let protected_before = b.paired_export_since(&VersionVector::default().encode()).unwrap();
    for blob in [mixed, updates, forged.export(ExportMode::Snapshot).unwrap()] {
        assert!(b.apply(&blob).is_err());
        assert!(b.paired_apply(&blob).is_err());
        let vv = VersionVector::default().encode();
        let offer = [&(vv.len() as u32).to_le_bytes()[..], &vv, &blob].concat();
        assert!(answer(&b, &offer).is_err());
        assert_eq!(b.export_all().unwrap(), public_before);
        assert_eq!(
            b.paired_export_since(&VersionVector::default().encode()).unwrap(),
            protected_before
        );
        assert!(b.get("ok").is_none());
    }
}
