use super::*;
use eon_foundation::journal::replay;

#[test]
fn durable_root_snapshot_precedes_edits_without_publishing_a_tree_listing() {
    let f = Fixture::new();
    let root = f.root();
    dir(&root.join("src"));
    dir(&root.join("empty"));
    file(&root.join("src/operator-file"), b"\xff\0");
    file(&root.join(".project"), b"");
    symlink(&f.0, root.join(".git")).unwrap(); // Never inspect gitdir links or files.
    dir(&root.join(STORE));
    file(&root.join(STORE).join("unrelated-object"), b"not in tree");
    let w = f.open();
    let journal = f.journal();
    let snapshot = w.snapshot(&journal).unwrap();
    let edit = w
        .edit("src/operator-file", digest(b"\xff\0"), b"edited", &journal)
        .unwrap();
    assert!(edit.journal_seq > snapshot.journal_seq);
    drop(journal);
    drop(w);

    let history = replay(f.0.join("journal")).unwrap();
    assert_eq!(history.len(), 2);
    let frame = &history[0];
    assert_eq!(frame.seq_no, snapshot.journal_seq);
    let relative = format!("{STORE}/{}", snapshot.snapshot_id);
    assert!(f.open().read_range(&relative, 0, MAX_READ_BYTES).is_err());
    let archive = root.join(relative);
    assert_eq!(host::metadata(&archive).unwrap().mode() & 0o7777, 0o400);
    let encoded = host::read(archive).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&encoded).unwrap(),
        serde_json::json!({".project": [], "src": null, "empty": null, "src/operator-file": [255, 0]})
    );
    let meta = host::metadata(root).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&frame.payload).unwrap(),
        serde_json::json!({
            "type": "workspace.snapshot.v1", "root_dev": meta.dev(), "root_ino": meta.ino(),
            "snapshot_id": snapshot.snapshot_id, "sha256": hex_digest(digest(&encoded))
        })
    );
}

#[test]
fn ordinary_mixed_case_git_file_rejects_the_whole_snapshot() {
    let f = Fixture::new();
    let root = f.root();
    let nested = root.join("nested");
    dir(&nested);
    file(&root.join("legitimate"), b"allowed");
    file(&nested.join(".Git"), b"ordinary file");
    // On a case-sensitive filesystem both entries can exist independently.
    if !nested.join(".git").try_exists().unwrap() {
        dir(&nested.join(".git"));
    }

    let journal = f.journal();
    assert!(matches!(
        f.open().snapshot(&journal),
        Err(WorkspaceError::Invalid("reserved snapshot name"))
    ));
    assert_eq!(host::read(nested.join(".Git")).unwrap(), b"ordinary file");
    assert_eq!(host::read_dir(root.join(STORE)).unwrap().count(), 0);
    drop(journal);
    assert!(replay(f.0.join("journal")).unwrap().is_empty());
}

#[test]
fn denied_tree_entry_never_publishes_a_partial_snapshot() {
    let f = Fixture::new();
    let root = f.root();
    let w = f.open();
    let journal = f.journal();
    dir(&root.join("nested"));
    file(&root.join("legitimate"), b"allowed");
    let bad = root.join("nested/item");
    let canary = f.0.join("outside");
    file(&canary, b"private");
    let deny = || {
        assert!(w.snapshot(&journal).is_err());
        assert_eq!(host::read_dir(root.join(STORE)).unwrap().count(), 0);
    };
    symlink(&f.0, &bad).unwrap();
    deny();
    host::remove_file(&bad).unwrap();
    host::hard_link(&canary, &bad).unwrap();
    deny();
    host::remove_file(&bad).unwrap();
    fs::mknodat(fs::CWD, &bad, fs::FileType::Fifo, Mode::RUSR, 0).unwrap();
    deny();
    host::remove_file(&bad).unwrap();
    file(&bad, b"untrusted");
    host::set_permissions(&bad, host::Permissions::from_mode(0o666)).unwrap();
    deny();
    host::remove_file(&bad).unwrap();
    host::set_permissions(root.join("nested"), host::Permissions::from_mode(0o777)).unwrap();
    deny();
    assert_eq!(host::read(canary).unwrap(), b"private");
    drop(journal);
    assert!(replay(f.0.join("journal")).unwrap().is_empty());
}

#[test]
fn tree_snapshot_bounds_files_aggregate_entries_and_depth() {
    use super::super::snapshot::{MAX_DEPTH, MAX_ENTRIES, MAX_TOTAL_BYTES};
    let f = Fixture::new();
    let root = f.root();
    let w = f.open();
    let journal = f.journal();
    let deny = |reason| {
        assert!(matches!(
            w.snapshot(&journal),
            Err(WorkspaceError::Invalid(message)) if message == reason
        ));
        assert_eq!(host::read_dir(root.join(STORE)).unwrap().count(), 0);
    };
    let sparse = |path: &Path, size| {
        file(path, b"");
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_len(size)
            .unwrap();
    };
    let large = root.join("large");
    sparse(&large, MAX_EDIT_BYTES as u64 + 1);
    deny("file exceeds edit limit");
    host::remove_file(large).unwrap();
    for i in 0..=MAX_TOTAL_BYTES / MAX_EDIT_BYTES {
        sparse(&root.join(i.to_string()), MAX_EDIT_BYTES as u64);
    }
    deny("snapshot content exceeds limit");
    for i in 0..=MAX_TOTAL_BYTES / MAX_EDIT_BYTES {
        host::remove_file(root.join(i.to_string())).unwrap();
    }
    dir(&root.join("many"));
    for i in 0..MAX_ENTRIES {
        file(&root.join("many").join(i.to_string()), b"");
    }
    deny("snapshot tree exceeds limit");
    host::remove_dir_all(root.join("many")).unwrap();
    let mut nested = root.clone();
    for _ in 0..=MAX_DEPTH {
        nested.push("sub");
        dir(&nested);
    }
    deny("snapshot tree exceeds limit");
    drop(journal);
    assert!(replay(f.0.join("journal")).unwrap().is_empty());
}

#[test]
fn snapshot_and_nested_edits_exclude_each_other_through_durable_preparation() {
    let f = Fixture::new();
    let root = f.root();
    dir(&root.join("src"));
    let item = "src/item";
    file(&root.join(item), b"old");
    let w = f.open();
    let independent = f.open();
    let journal = f.journal();
    let result = w.snapshot_with_record(|payload| {
        let frame: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        let object = root
            .join(STORE)
            .join(frame["snapshot_id"].as_str().unwrap());
        assert!(object.is_file(), "private bytes must precede the frame");
        assert!(matches!(
            independent.edit(item, digest(b"old"), b"new", &journal),
            Err(WorkspaceError::Conflict)
        ));
        // Even a lost acknowledgement must leave the pre-write snapshot available.
        journal.append_durable(payload)?;
        Err(eon_foundation::EonError::InternalError("lost ack".into()))
    });
    assert!(matches!(result, Err(WorkspaceError::Journal(_))));
    assert_eq!(host::read(root.join(item)).unwrap(), b"old");
    w.edit_with_record(item, digest(b"old"), b"new", |record| {
        assert!(matches!(
            independent.snapshot(&journal),
            Err(WorkspaceError::Conflict)
        ));
        journal.append_durable(record)
    })
    .unwrap();
    assert_eq!(host::read(root.join(item)).unwrap(), b"new");
    drop(journal);
    let frames = replay(f.0.join("journal")).unwrap();
    assert_eq!(frames.len(), 2);
    let record: serde_json::Value = serde_json::from_slice(&frames[0].payload).unwrap();
    let object = root
        .join(STORE)
        .join(record["snapshot_id"].as_str().unwrap());
    assert_eq!(
        hex_digest(digest(&host::read(object).unwrap())),
        record["sha256"]
    );
}

#[test]
fn a_durable_frame_is_not_a_receipt_for_a_replaced_tree_object() {
    let f = Fixture::new();
    let root = f.root();
    file(&root.join("item"), b"original");
    let journal = f.journal();
    let result = f.open().snapshot_with_record(|payload| {
        let frame: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        let object = root
            .join(STORE)
            .join(frame["snapshot_id"].as_str().unwrap());
        let seq = journal.append_durable(payload)?;
        host::rename(&object, root.join(STORE).join("moved")).unwrap();
        file(&object, b"wrong object");
        Ok(seq)
    });
    assert!(matches!(result, Err(WorkspaceError::Conflict)));
    assert_eq!(host::read(root.join("item")).unwrap(), b"original");
    drop(journal);
    assert_eq!(replay(f.0.join("journal")).unwrap().len(), 1);
}
