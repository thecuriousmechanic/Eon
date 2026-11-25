use super::*;
use std::fs as host;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};
use std::path::PathBuf;
use std::sync::{Arc, Barrier};
use std::thread;

mod recovery_tests;
mod snapshot_tests;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        // A checkout may be group-writable. Keep the journal and workspace
        // under a vetted operator-owned directory, without changing its mode.
        let home = std::env::home_dir().expect("workspace tests need an operator home");
        let base = eon_foundation::owned_dir::OwnedDirectory::open(&home, false).unwrap();
        let mut id = [0; 16];
        getrandom::fill(&mut id).unwrap();
        let name = format!(".eon-workspace-test-{:032x}", u128::from_le_bytes(id));
        dir(&base.path().join(&name));
        let path = home.join(name);
        dir(&path.join("workspace"));
        Self(path)
    }

    fn root(&self) -> PathBuf {
        self.0.join("workspace")
    }
    fn open(&self) -> Workspace {
        Workspace::open(self.root()).unwrap()
    }
    fn journal(&self) -> GroupCommitJournal {
        GroupCommitJournal::spawn(self.0.join("journal"), 16).unwrap()
    }
}

fn dir(path: &Path) {
    host::DirBuilder::new().mode(0o700).create(path).unwrap();
}

fn file(path: &Path, bytes: &[u8]) {
    host::write(path, bytes).unwrap();
    host::set_permissions(path, host::Permissions::from_mode(0o600)).unwrap();
}

impl Drop for Fixture {
    fn drop(&mut self) {
        host::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn ranges_are_binary_bounded_and_do_not_require_reading_the_whole_file() {
    let f = Fixture::new();
    file(&f.root().join("blob"), b"\xffabc\x00");
    let w = f.open();
    let r = w.read_range("blob", 1, 3).unwrap();
    assert_eq!((r.bytes.as_slice(), r.file_len), (&b"abc"[..], 5));
    assert_eq!(
        r.sha256,
        [
            0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
            0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
            0xf2, 0x00, 0x15, 0xad,
        ]
    );
    assert_eq!(w.read_range("blob", 4, 10).unwrap().bytes, [0]);
    assert!(w.read_range("blob", 5, 1).unwrap().bytes.is_empty());
    assert!(w.read_range("blob", 6, 0).is_err());
    assert!(w.read_range("blob", 0, MAX_READ_BYTES + 1).is_err());
    assert!(w.read_range("blob", u64::MAX, 1).is_err());
    let big = File::create(f.root().join("sparse")).unwrap();
    big.set_len(1 << 30).unwrap();
    assert_eq!(
        w.read_range("sparse", (1 << 30) - 2, 4).unwrap().bytes,
        [0; 2]
    );
}

#[test]
fn atomic_edit_keeps_private_durable_preimage_and_records_metadata_only() {
    let f = Fixture::new();
    let root = f.root();
    dir(&root.join("src"));
    let path = root.join("src/item");
    let original = b"private binary preimage\xff";
    file(&path, original);
    host::set_permissions(&path, host::Permissions::from_mode(0o755)).unwrap();
    let mut old_handle = File::open(&path).unwrap();
    let w = f.open();
    let journal = f.journal();
    let receipt = w
        .edit("src/item", digest(original), b"new", &journal)
        .unwrap();
    assert_eq!(receipt.sha256, digest(b"new"));
    assert_eq!(host::read(&path).unwrap(), b"new");
    assert_eq!(host::metadata(&path).unwrap().mode() & 0o777, 0o700);
    let mut old = vec![];
    old_handle.read_to_end(&mut old).unwrap();
    assert_eq!(old, original);
    let snapshot = root.join(STORE).join(&receipt.snapshot_id);
    assert_eq!(host::read(&snapshot).unwrap(), original);
    assert_eq!(host::metadata(snapshot).unwrap().mode() & 0o077, 0);
    assert!(
        w.read_range(&format!("{STORE}/{}", receipt.snapshot_id), 0, 64)
            .is_err()
    );
    assert!(matches!(
        w.edit("src/item", digest(original), b"stale", &journal),
        Err(WorkspaceError::Conflict)
    ));
    drop(journal);
    let records = eon_foundation::journal::replay(f.0.join("journal")).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].seq_no, receipt.journal_seq);
    let record: serde_json::Value = serde_json::from_slice(&records[0].payload).unwrap();
    assert_eq!(record["type"], "workspace.edit.prepared.v1");
    assert_eq!(
        record["before_sha256"],
        format!("{:x}", Sha256::digest(original))
    );
    assert_eq!(
        record["after_sha256"],
        format!("{:x}", Sha256::digest(b"new"))
    );
    assert_eq!(record["snapshot_id"], receipt.snapshot_id);
    let text = std::str::from_utf8(&records[0].payload).unwrap();
    assert!(!text.contains("private binary preimage") && !text.contains("src/item"));
    assert_eq!(host::read_dir(root.join(STORE)).unwrap().count(), 1);
}

#[test]
fn absolute_traversal_symlinks_hardlinks_and_special_files_are_denied() {
    let f = Fixture::new();
    let root = f.root();
    let outside = f.0.join("canary");
    file(&outside, b"canary");
    file(&root.join("ok"), b"canary");
    symlink(&outside, root.join("leaf")).unwrap();
    symlink(&f.0, root.join("ancestor")).unwrap();
    symlink(root.join("missing"), root.join("dangling")).unwrap();
    host::hard_link(&outside, root.join("hard")).unwrap();
    dir(&root.join("directory"));
    fs::mknodat(
        fs::CWD,
        root.join("fifo"),
        fs::FileType::Fifo,
        Mode::RUSR,
        0,
    )
    .unwrap();
    let w = f.open();
    let j = f.journal();
    for bad in [
        "",
        "/canary",
        "../canary",
        "./ok",
        "a//b",
        "ok/",
        "a\\b",
        "a\0b",
        "leaf",
        "ancestor/canary",
        "dangling",
        "hard",
        "directory",
        "fifo",
    ] {
        assert!(w.read_range(bad, 0, 1).is_err(), "read {bad:?}");
        assert!(
            w.edit(bad, digest(b"canary"), b"x", &j).is_err(),
            "edit {bad:?}"
        );
    }
    assert_eq!(host::read(outside).unwrap(), b"canary");
    assert!(!root.join(STORE).exists());
    symlink(&root, f.0.join("alias")).unwrap();
    assert!(Workspace::open(f.0.join("alias")).is_err());
}

#[test]
fn edit_limits_and_permissions_fail_without_creating_snapshots() {
    let f = Fixture::new();
    let root = f.root();
    file(&root.join("item"), b"x");
    let w = f.open();
    let j = f.journal();
    let large = vec![0; MAX_EDIT_BYTES + 1];
    assert!(w.edit("item", digest(b"x"), &large, &j).is_err());
    file(&root.join("large"), &large);
    assert!(w.edit("large", digest(&large), b"x", &j).is_err());
    assert!(w.edit("missing", digest(b""), b"x", &j).is_err());
    host::set_permissions(root.join("item"), host::Permissions::from_mode(0o444)).unwrap();
    assert!(w.edit("item", digest(b"x"), b"y", &j).is_err());
    dir(&root.join("shared"));
    host::set_permissions(root.join("shared"), host::Permissions::from_mode(0o777)).unwrap();
    dir(&root.join("shared/nested"));
    file(&root.join("shared/nested/item"), b"x");
    assert!(
        w.edit("shared/nested/item", digest(b"x"), b"y", &j)
            .is_err()
    );
    assert!(!root.join(STORE).exists());
}

#[test]
fn a_symlink_or_nonprivate_snapshot_directory_cannot_receive_preimages() {
    let f = Fixture::new();
    let root = f.root();
    let trap = f.0.join("trap");
    dir(&trap);
    file(&root.join("item"), b"old");
    symlink(&trap, root.join(STORE)).unwrap();
    let w = f.open();
    let j = f.journal();
    assert!(w.edit("item", digest(b"old"), b"new", &j).is_err());
    assert_eq!(host::read_dir(&trap).unwrap().count(), 0);
    host::remove_file(root.join(STORE)).unwrap();
    dir(&root.join(STORE));
    host::set_permissions(root.join(STORE), host::Permissions::from_mode(0o755)).unwrap();
    assert!(w.edit("item", digest(b"old"), b"new", &j).is_err());
    assert_eq!(host::read(root.join("item")).unwrap(), b"old");
    assert_eq!(host::read_dir(root.join(STORE)).unwrap().count(), 0);
}

#[test]
fn journal_failure_precedes_rename_and_preserves_a_recoverable_preimage() {
    let f = Fixture::new();
    let root = f.root();
    file(&root.join("item"), b"old");
    let result = f
        .open()
        .edit_with_record("item", digest(b"old"), b"new", |record| {
            let event: serde_json::Value = serde_json::from_slice(&record).unwrap();
            let snapshot = root
                .join(STORE)
                .join(event["snapshot_id"].as_str().unwrap());
            assert_eq!(host::read(snapshot).unwrap(), b"old");
            assert_eq!(host::read_dir(root.join(STORE)).unwrap().count(), 1);
            assert_eq!(host::read(root.join("item")).unwrap(), b"old");
            Err(eon_foundation::EonError::InternalError(
                "injected sync failure".into(),
            ))
        });
    assert!(matches!(result, Err(WorkspaceError::Journal(_))));
    assert_eq!(host::read(root.join("item")).unwrap(), b"old");
    assert_eq!(host::read_dir(root.join(STORE)).unwrap().count(), 1);
}

#[test]
fn namespace_and_content_changes_during_prepare_do_not_commit_to_moved_targets() {
    let f = Fixture::new();
    let root = f.root();
    dir(&root.join("sub"));
    file(&root.join("sub/item"), b"old");
    let w = f.open();
    let path = "sub/item";
    let result = w.edit_with_record(path, digest(b"old"), b"new", |_| {
        host::write(root.join(path), b"external writer").unwrap();
        Ok(1)
    });
    assert!(matches!(result, Err(WorkspaceError::Conflict)));
    assert_eq!(host::read(root.join(path)).unwrap(), b"external writer");

    file(&f.0.join("item"), b"outside");
    let result = w.edit_with_record(path, digest(b"external writer"), b"new", |_| {
        host::rename(root.join("sub"), f.0.join("moved")).unwrap();
        symlink(&f.0, root.join("sub")).unwrap();
        Ok(2)
    });
    assert!(result.is_err());
    assert_eq!(host::read(f.0.join("item")).unwrap(), b"outside");
    assert_eq!(
        host::read(f.0.join("moved/item")).unwrap(),
        b"external writer"
    );
}

#[test]
fn directory_lock_lifetime_is_the_edit_not_an_inherited_descriptor() {
    let f = Fixture::new();
    let w = f.open();
    let parent = w.edit_parent(".").unwrap();
    let other = w.edit_parent(".").unwrap();
    let lock = EditLock::acquire(&parent).unwrap();
    // dup and fork both retain the same open file description.
    let inherited = parent.try_clone().unwrap();
    assert!(matches!(
        EditLock::acquire(&other),
        Err(WorkspaceError::Conflict)
    ));
    drop(lock);
    drop(parent);
    let _next = EditLock::acquire(&other).unwrap();
    drop(inherited);
}

#[test]
fn independent_handles_with_same_expected_digest_have_one_winner() {
    let f = Fixture::new();
    file(&f.root().join("item"), b"old");
    let journal = Arc::new(f.journal());
    let barrier = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8u8)
        .map(|id| {
            let w = f.open();
            let j = journal.clone();
            let b = barrier.clone();
            thread::spawn(move || {
                b.wait();
                (id, w.edit("item", digest(b"old"), &[id], &j))
            })
        })
        .collect();
    let mut winners = vec![];
    for h in handles {
        match h.join().unwrap() {
            (id, Ok(_)) => winners.push(id),
            (_, Err(WorkspaceError::Conflict)) => (),
            (_, Err(e)) => panic!("unexpected failure: {e}"),
        }
    }
    assert_eq!(winners.len(), 1);
    assert_eq!(host::read(f.root().join("item")).unwrap(), winners);
    assert_eq!(host::read_dir(f.root().join(STORE)).unwrap().count(), 1);
}

#[test]
fn private_staging_substitution_fails_before_rename() {
    let f = Fixture::new();
    let root = f.root();
    file(&root.join("item"), b"old");
    file(&f.0.join("canary"), b"outside");
    let w = f.open();
    let parent = w.edit_parent(".").unwrap();
    let store = w.snapshot_directory().unwrap();
    let mut stage = PrivateFile::create(&store, "stage").unwrap();
    let version = stage
        .persist_bytes(b"new", Mode::RUSR | Mode::WUSR)
        .unwrap();
    let name = root.join(STORE).join(&stage.name);
    host::remove_file(&name).unwrap();
    symlink(f.0.join("canary"), name).unwrap();
    assert!(stage.install(&version, &parent, "item").is_err());
    assert_eq!(host::read(root.join("item")).unwrap(), b"old");
    assert_eq!(host::read(f.0.join("canary")).unwrap(), b"outside");
}

#[test]
fn range_reads_never_follow_a_concurrently_substituted_directory_symlink() {
    let f = Fixture::new();
    let root = f.root();
    dir(&root.join("sub"));
    file(&root.join("sub/item"), b"inside");
    file(&f.0.join("item"), b"outside");
    symlink(&f.0, root.join("alias")).unwrap();
    let w = f.open();
    let barrier = Barrier::new(2);
    thread::scope(|s| {
        s.spawn(|| {
            let fd = File::open(&root).unwrap();
            barrier.wait();
            for _ in 0..512 {
                fs::renameat_with(&fd, "sub", &fd, "alias", fs::RenameFlags::EXCHANGE).unwrap();
            }
        });
        barrier.wait();
        for _ in 0..512 {
            if let Ok(read) = w.read_range("sub/item", 0, MAX_READ_BYTES) {
                assert_eq!(read.bytes, b"inside");
            }
        }
    });
    assert_eq!(w.read_range("sub/item", 0, 8).unwrap().bytes, b"inside");
}

#[test]
fn root_is_pinned_across_rename_and_path_substitution() {
    let f = Fixture::new();
    let root = f.root();
    file(&root.join("item"), b"original");
    let w = Workspace::from_directory(File::open(&root).unwrap()).unwrap();
    host::rename(&root, f.0.join("renamed")).unwrap();
    dir(&root);
    file(&root.join("item"), b"decoy");
    assert_eq!(w.read_range("item", 0, 20).unwrap().bytes, b"original");
    w.edit("item", digest(b"original"), b"updated", &f.journal())
        .unwrap();
    assert_eq!(host::read(f.0.join("renamed/item")).unwrap(), b"updated");
    assert_eq!(host::read(root.join("item")).unwrap(), b"decoy");
}
