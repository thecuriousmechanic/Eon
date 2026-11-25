use super::*;
use eon_foundation::journal::{Frame, replay};

const ITEM: &str = "operator-private-file";

fn records(f: &Fixture) -> Vec<Frame> {
    replay(f.0.join("journal")).unwrap()
}

// Drop all the writer's handles and recover only from durable state.
fn edited(original: &[u8], replacement: &[u8]) -> (Fixture, Frame, PathBuf) {
    let f = Fixture::new();
    file(&f.root().join(ITEM), original);
    let r = f
        .open()
        .edit(ITEM, digest(original), replacement, &f.journal())
        .unwrap();
    let mut frames = records(&f);
    assert_eq!(frames.len(), 1);
    let frame = frames.pop().unwrap();
    assert_eq!(frame.seq_no, r.journal_seq);
    let snapshot = f.root().join(STORE).join(r.snapshot_id);
    (f, frame, snapshot)
}

#[test]
fn restart_restores_binary_preimage_atomically_and_journals_the_reverse_edit() {
    let original = vec![0xfe; MAX_EDIT_BYTES];
    let after = b"private replacement\x00\xff";
    let (f, frame, snapshot) = edited(&original, after);
    let target = f.root().join(ITEM);
    let mut old_handle = File::open(&target).unwrap();
    let journal = f.journal();
    let w = f.open();
    let receipt = w
        .restore_preimage(ITEM, digest(after), &frame, &journal)
        .unwrap();

    assert_eq!(receipt.sha256, digest(&original));
    assert_eq!(host::read(&target).unwrap(), original);
    assert_eq!(host::metadata(&target).unwrap().mode() & 0o7777, 0o600);
    let mut prior = Vec::new();
    old_handle.read_to_end(&mut prior).unwrap();
    assert_eq!(prior, after);
    assert_eq!(host::read(snapshot).unwrap(), original);
    let undo = f.root().join(STORE).join(&receipt.snapshot_id);
    assert_eq!(host::metadata(&undo).unwrap().mode() & 0o7777, 0o400);
    assert_eq!(host::read(undo).unwrap(), after);
    assert!(matches!(
        w.restore_preimage(ITEM, digest(after), &frame, &journal),
        Err(WorkspaceError::Conflict)
    ));
    drop(journal);
    let history = records(&f);
    assert_eq!(history.len(), 2);
    assert_eq!(history[1].seq_no, receipt.journal_seq);
    let event: serde_json::Value = serde_json::from_slice(&history[1].payload).unwrap();
    assert_eq!(event["snapshot_id"], receipt.snapshot_id);
    assert_eq!(
        event["before_sha256"],
        format!("{:x}", Sha256::digest(after))
    );
    assert_eq!(
        event["after_sha256"],
        format!("{:x}", Sha256::digest(&original))
    );
    let text = std::str::from_utf8(&history[1].payload).unwrap();
    assert!(!text.contains(ITEM) && !text.contains("private replacement"));
}

#[test]
fn existing_v1_entry_restores_without_an_edit_receipt_or_a_format_migration() {
    let f = Fixture::new();
    let root = f.root();
    let meta = host::metadata(&root).unwrap();
    file(&root.join(ITEM), b"abc");
    let id = "preimage-0123456789abcdef0123456789abcdef";
    dir(&root.join(STORE));
    let snapshot = root.join(STORE).join(id);
    file(&snapshot, b"");
    host::set_permissions(snapshot, host::Permissions::from_mode(0o400)).unwrap();
    // Independent v1 layout, including known SHA-256 vectors for "" and "abc".
    let payload = serde_json::json!({
        "snapshot_id": id,
        "before_sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "after_sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        "type": "workspace.edit.prepared.v1",
        "path_sha256": format!("{:x}", Sha256::digest(ITEM.as_bytes())),
        "root_ino": meta.ino(),
        "root_dev": meta.dev(),
    });
    f.journal()
        .append_durable(payload.to_string().into_bytes())
        .unwrap();
    let entry = records(&f).pop().unwrap();
    f.open()
        .restore_preimage(ITEM, digest(b"abc"), &entry, &f.journal())
        .unwrap();
    assert_eq!(host::read(root.join(ITEM)).unwrap(), b"");
}

#[test]
fn prepared_but_not_installed_missing_and_diverged_targets_are_not_rolled_back() {
    let f = Fixture::new();
    let path = f.root().join(ITEM);
    file(&path, b"old");
    let journal = f.journal();
    let w = f.open();
    let result = w.edit_with_record(ITEM, digest(b"old"), b"new", |record| {
        journal.append_durable(record)?;
        Err(eon_foundation::EonError::InternalError("lost ack".into()))
    });
    assert!(matches!(result, Err(WorkspaceError::Journal(_))));
    drop(journal);
    let frame = records(&f).pop().unwrap();
    let journal = f.journal();
    for current in [b"old".as_slice(), b"independent change"] {
        file(&path, current);
        let version = host::metadata(&path).unwrap();
        // Even the correct current digest is not authority to replace a divergent file.
        for expected in [digest(current), digest(b"new")] {
            assert!(
                w.restore_preimage(ITEM, expected, &frame, &journal)
                    .is_err()
            );
            assert_eq!(host::read(&path).unwrap(), current);
            assert!(same_version(&version, &host::metadata(&path).unwrap()));
        }
    }
    host::remove_file(&path).unwrap();
    assert!(
        w.restore_preimage(ITEM, digest(b"new"), &frame, &journal)
            .is_err()
    );
    assert!(!path.exists());
    assert_eq!(host::read_dir(f.root().join(STORE)).unwrap().count(), 1);
    assert_eq!(records(&f), [frame]);
}

#[test]
fn restore_is_bound_to_the_root_and_path_and_obeys_the_edit_lock() {
    let (f, frame, snapshot) = edited(b"old", b"new");
    let journal = f.journal();
    let other = Fixture::new();
    // Both roots have the same valid preimage name/bytes, path and current digest.
    dir(&other.root().join(STORE));
    host::copy(
        &snapshot,
        other.root().join(STORE).join(snapshot.file_name().unwrap()),
    )
    .unwrap();
    file(&other.root().join(ITEM), b"new");
    file(&f.root().join("sibling"), b"new");
    let expected = digest(b"new");
    assert!(
        other
            .open()
            .restore_preimage(ITEM, expected, &frame, &journal)
            .is_err()
    );
    let w = f.open();
    assert!(
        w.restore_preimage("sibling", expected, &frame, &journal)
            .is_err()
    );
    let parent = File::open(f.root()).unwrap();
    fs::flock(&parent, fs::FlockOperation::LockExclusive).unwrap();
    assert!(matches!(
        w.restore_preimage(ITEM, expected, &frame, &journal),
        Err(WorkspaceError::Conflict)
    ));
    drop(parent);
    let target = f.root().join(ITEM);
    host::remove_file(&target).unwrap();
    symlink(other.root().join(ITEM), &target).unwrap();
    assert!(
        w.restore_preimage(ITEM, expected, &frame, &journal)
            .is_err()
    );
    assert!(target.is_symlink());
    assert_eq!(host::read(other.root().join(ITEM)).unwrap(), b"new");
    assert_eq!(host::read(f.root().join("sibling")).unwrap(), b"new");
    assert_eq!(host::read_dir(f.root().join(STORE)).unwrap().count(), 1);
    assert_eq!(host::read_dir(other.root().join(STORE)).unwrap().count(), 1);
    assert_eq!(records(&f), [frame]);
}

#[test]
fn malformed_oversized_and_unrelated_frames_cannot_authorize_restoration() {
    let (f, frame, snapshot) = edited(b"old", b"new");
    let journal = f.journal();
    let w = f.open();
    let deny = |payload| {
        let candidate = Frame {
            payload,
            ..frame.clone()
        };
        assert!(
            w.restore_preimage(ITEM, digest(b"new"), &candidate, &journal)
                .is_err()
        );
    };
    deny(b"null".to_vec());
    deny(frame.payload[..frame.payload.len() - 1].to_vec());
    let mut too_big = frame.payload.clone();
    too_big.resize(4096, b' ');
    deny(too_big);
    let mut duplicate = br#"{"type":"unrelated","#.to_vec();
    duplicate.extend_from_slice(&frame.payload[1..]);
    deny(duplicate);
    let stage = "stage-0123456789abcdef0123456789abcdef";
    host::copy(&snapshot, snapshot.with_file_name(stage)).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&frame.payload).unwrap();
    for (field, value) in [
        ("type", "workspace.edit.committed.v1"),
        ("unknown", "field"),
        ("snapshot_id", stage),
        (
            "snapshot_id",
            &format!("./{}", snapshot.file_name().unwrap().to_str().unwrap()),
        ),
        ("before_sha256", &"f".repeat(64)),
        ("after_sha256", &"0".repeat(64)),
    ] {
        let mut bad = v.clone();
        bad[field] = serde_json::json!(value);
        deny(bad.to_string().into_bytes());
    }
    assert_eq!(host::read(f.root().join(ITEM)).unwrap(), b"new");
    assert_eq!(host::read_dir(f.root().join(STORE)).unwrap().count(), 2);
    assert_eq!(records(&f), [frame]);
}

#[test]
fn restore_rejects_missing_unsafe_or_corrupted_private_objects_without_writing() {
    let (f, frame, snapshot) = edited(b"old", b"new");
    let journal = f.journal();
    let w = f.open();
    let store = f.root().join(STORE);
    let target = f.root().join(ITEM);
    let version = host::metadata(&target).unwrap();
    let deny = || {
        assert!(
            w.restore_preimage(ITEM, digest(b"new"), &frame, &journal)
                .is_err()
        );
        assert_eq!(host::read(&target).unwrap(), b"new");
        assert!(same_version(&version, &host::metadata(&target).unwrap()));
    };
    let moved = f.0.join("relocated");
    host::rename(&store, &moved).unwrap();
    deny();
    assert!(!store.exists());
    symlink(&moved, &store).unwrap();
    deny();
    host::remove_file(&store).unwrap();
    host::rename(moved, &store).unwrap();
    host::set_permissions(&store, host::Permissions::from_mode(0o755)).unwrap();
    deny();
    host::set_permissions(&store, host::Permissions::from_mode(0o700)).unwrap();
    for mode in [0o444, 0o600] {
        host::set_permissions(&snapshot, host::Permissions::from_mode(mode)).unwrap();
        deny();
    }
    let canary = f.0.join("outside");
    host::rename(&snapshot, &canary).unwrap();
    host::set_permissions(&canary, host::Permissions::from_mode(0o400)).unwrap();
    deny();
    symlink(&canary, &snapshot).unwrap();
    deny();
    host::remove_file(&snapshot).unwrap();
    host::hard_link(&canary, &snapshot).unwrap();
    deny();
    host::remove_file(&snapshot).unwrap();
    fs::mknodat(fs::CWD, &snapshot, fs::FileType::Fifo, Mode::RUSR, 0).unwrap();
    deny();
    host::remove_file(&snapshot).unwrap();
    file(&snapshot, b"bad");
    let fd = File::options().write(true).open(&snapshot).unwrap();
    host::set_permissions(&snapshot, host::Permissions::from_mode(0o400)).unwrap();
    deny(); // Same length, mode and owner, wrong digest.
    fd.set_len(MAX_EDIT_BYTES as u64 + 1).unwrap();
    deny();
    assert_eq!(host::read(&canary).unwrap(), b"old");
    assert_eq!(host::read_dir(store).unwrap().count(), 1);
    assert_eq!(records(&f), [frame]);
}
