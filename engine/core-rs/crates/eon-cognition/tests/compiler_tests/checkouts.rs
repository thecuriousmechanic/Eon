use std::fs::{self, File};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use eon_cognition::compiler::{
    Attention, CONFIRMATION_WINDOW, CandidateDecision, CheckoutBacking, MAX_READ_BYTES,
    ReadWorkspace, WorktreeStore,
};
use eon_kernel::TaskId;
use rustix::io::Errno;

use super::{Fixture, capture, checkout_candidates, mkdir, objective, write};

fn entries(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect()
}

fn inode(path: &Path) -> (u64, u64) {
    let m = path.symlink_metadata().unwrap();
    (m.dev(), m.ino())
}

// Includes anonymous extent files; the store dirfd itself is allowed to survive.
fn open_children(scratch: &Path) -> usize {
    let root = fs::canonicalize(scratch).unwrap();
    entries(Path::new("/proc/self/fd"))
        .into_iter()
        .filter_map(|fd| fs::read_link(fd).ok())
        .filter(|path| path.starts_with(&root) && path != &root)
        .count()
}

#[test]
fn forced_exit_with_edited_branches_leaves_no_named_extent() {
    const CHILD: &str = "EON_TEST_CRASH_CHECKOUT_FIXTURE";
    const STAGED: i32 = 86;
    if let Some(path) = std::env::var_os(CHILD) {
        let path = PathBuf::from(path);
        let ws = ReadWorkspace::from_directory(
            File::open(path.join("workspace")).unwrap(),
            &["src/A", "src/B"],
        )
        .unwrap();
        let store =
            WorktreeStore::from_directory(File::open(path.join("scratch")).unwrap()).unwrap();
        let mut branches =
            checkout_candidates(&objective("read file src/A or src/B"), &ws, &store).unwrap();
        let ids: Vec<_> = branches.choices().map(|c| c.id.clone()).collect();
        for id in ids {
            branches.overwrite(&id, 1, b"edited").unwrap();
        }
        // Do not unwind: all lower/upper descriptors and directory guards are live.
        std::process::exit(STAGED);
    }

    let f = Fixture::new();
    mkdir(&f.root().join("src"));
    write(&f.root().join("src/A"), b"captured first extent");
    write(&f.root().join("src/B"), b"captured second extent");
    let scratch = f.path().join("scratch");
    mkdir(&scratch);
    let unrelated = scratch.join("operator-file");
    write(&unrelated, b"keep");
    let original = inode(&unrelated);
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg(std::thread::current().name().unwrap())
        .env(CHILD, fs::canonicalize(f.path()).unwrap())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(STAGED));

    // Empty private directories can outlive their guards, but no named data can.
    let mut pending = entries(&scratch);
    while let Some(path) = pending.pop() {
        if path != unrelated {
            assert!(path.symlink_metadata().unwrap().is_dir(), "{path:?}");
            pending.extend(entries(&path));
        }
    }
    assert_eq!(inode(&unrelated), original);
    assert_eq!(fs::read(unrelated).unwrap(), b"keep");
}

#[test]
fn two_edits_to_one_captured_range_are_private_even_after_source_replacement() {
    let f = Fixture::new();
    let path = f.root().join("Data");
    let original = f.root().join("Moved");
    let outside = f.path().join("outside");
    write(&path, b"before-12345-after");
    write(&outside, b"not captured");
    let ws = f.workspace(&["Data"]);
    let store = f.store();
    let intent = "read 5 bytes from Data at offset 7";
    let a = capture(&ws, intent).unwrap();
    let b = capture(&ws, intent).unwrap();
    drop(ws);
    fs::rename(&path, &original).unwrap();
    symlink(&outside, &path).unwrap();

    for (snapshot, index, expected) in [(a, 0, b"abc45"), (b, 1, b"12XYZ")] {
        let mut branches = snapshot.checkout_variants(&store).unwrap();
        let choices: Vec<_> = branches.choices().collect();
        assert_eq!(choices.len(), 2);
        assert_eq!(choices[0].request, choices[1].request);
        let ids = [choices[0].id.clone(), choices[1].id.clone()];
        assert_ne!(ids[0], ids[1]);
        for (off, bytes) in [
            (6, b"x".as_slice()),
            (12, b"x"),
            (11, b"xy"),
            (u64::MAX, b"x"),
            (7, b""),
        ] {
            assert!(branches.overwrite(&ids[0], off, bytes).is_err());
        }

        branches.overwrite(&ids[0], 7, b"abc").unwrap();
        branches.overwrite(&ids[1], 9, b"XYZ").unwrap();
        assert!(branches.overwrite(&ids[1], 7, b"!").is_err());
        let selected = branches
            .present(Attention::Verified)
            .unwrap()
            .select_checkout(
                CandidateDecision::Confirm(ids[index].clone()),
                Attention::Verified,
            )
            .unwrap();
        assert_eq!(selected.choice.id, ids[index]);
        assert_eq!(selected.choice.request.path, "Data");
        assert_eq!(
            selected.choice.overwrite,
            Some([7..10, 9..12][index].clone())
        );
        assert_eq!(selected.directory.read_file("Data").unwrap(), expected);
        assert_eq!(selected.file_len, 18);
    }
    assert_eq!(fs::read(original).unwrap(), b"before-12345-after");
    assert_eq!(fs::read(outside).unwrap(), b"not captured");
    assert!(path.is_symlink());
    assert_eq!(fs::read_dir(f.root()).unwrap().count(), 2);
}

fn probe_backing(parent: &Path) -> CheckoutBacking {
    let from = parent.join("probe-source");
    write(&from, b"probe");
    let probe = File::create_new(parent.join("probe-clone")).unwrap();
    match rustix::fs::ioctl_ficlone(&probe, File::open(from).unwrap()) {
        Ok(()) => CheckoutBacking::Reflink,
        Err(Errno::OPNOTSUPP | Errno::NOTTY | Errno::NOSYS) => CheckoutBacking::FileOverlay,
        Err(e) => panic!("unexpected reflink error: {e}"),
    }
}

#[test]
fn disjoint_read_clauses_make_real_private_extent_files_and_export_one_directory() {
    let f = Fixture::new();
    mkdir(&f.root().join("src"));
    let (a, b) = ("src/A", "src/B");
    write(&f.root().join(a), b"outsideALPHAoutside");
    write(&f.root().join(b), b"skipBRAVOtail");
    write(&f.root().join("src/Other"), b"approved but not requested");
    write(&f.root().join("src/.env"), b"protected");
    let ws = f.workspace(&[a, b, "src/Other"]);
    let store = f.store();
    let scratch = f.path().join("scratch");
    let backing = probe_backing(&f.path());
    let intent =
        objective("read 5 bytes from src/A at offset 7 or read 5 bytes from src/B at offset 4");
    let mut branches = checkout_candidates(&intent, &ws, &store).unwrap();
    let ids: Vec<_> = branches.choices().map(|c| c.id.clone()).collect();
    assert!(branches.choices().all(|c| c.backing == backing));
    let staged = entries(&scratch);
    assert_eq!(staged.len(), 2);
    assert_ne!(inode(&staged[0]), inode(&staged[1]));
    let assert_anonymous = || {
        for root in &staged {
            assert_eq!(entries(root), [root.join("src")]);
            assert_eq!(root.symlink_metadata().unwrap().mode() & 0o777, 0o700);
            assert!(entries(&root.join("src")).is_empty());
        }
    };
    assert_anonymous();
    branches.overwrite(&ids[0], 8, b"xx").unwrap();
    branches.overwrite(&ids[1], 6, b"yy").unwrap();
    assert_anonymous();

    let window = branches.present(Attention::Verified).unwrap();
    assert!(entries(&scratch).is_empty());
    let decision = CandidateDecision::Confirm(ids[1].clone());
    let selected = window
        .select_checkout(decision, Attention::Verified)
        .unwrap();
    let dir = &selected.directory;
    let [export]: [_; 1] = entries(&scratch).try_into().unwrap();
    assert_eq!(dir.metadata().unwrap().ino(), inode(&export).1);
    assert_eq!(dir.file_path(), b);
    assert_eq!(entries(&export), [export.join("src")]);
    assert_eq!(entries(&export.join("src")), [export.join(b)]);
    let m = dir.file_metadata(b).unwrap();
    assert_eq!((m.dev(), m.ino()), inode(&export.join(b)));
    assert_eq!((m.len(), m.nlink(), m.mode() & 0o777), (5, 1, 0o400));
    assert_eq!(fs::read(export.join(b)).unwrap(), b"BRyyO");
    for denied in [a, "src/Other", "src/.env", "../B", "/src/B", "src/../B"] {
        assert!(dir.file_metadata(denied).is_err());
        assert!(dir.read_file(denied).is_err());
    }
    // Even replacing the exact approved name cannot redirect the capability.
    fs::rename(export.join(b), export.join("saved")).unwrap();
    symlink(f.root().join("src/.env"), export.join(b)).unwrap();
    assert!(dir.read_file(b).is_err());
    fs::rename(export.join("saved"), export.join(b)).unwrap();
    assert_eq!(dir.read_file(b).unwrap(), b"BRyyO");
    drop(selected);
    assert!(entries(&scratch).is_empty());
    assert_eq!(open_children(&scratch), 0);
    assert_eq!(fs::read(f.root().join(a)).unwrap(), b"outsideALPHAoutside");
    assert_eq!(fs::read(f.root().join(b)).unwrap(), b"skipBRAVOtail");
}

#[test]
fn refuses_shared_or_source_local_scratch_including_on_export() {
    let f = Fixture::new();
    let ws = f.workspace(&["A"]);
    write(&f.root().join("A"), b"data");
    let subdir = f.root().join("nested");
    mkdir(&subdir);
    let snapshot = || capture(&ws, "read file A").unwrap();
    for path in [f.root(), subdir.clone()] {
        let store = WorktreeStore::from_directory(File::open(path).unwrap()).unwrap();
        assert!(snapshot().checkout_variants(&store).is_err());
    }
    assert!(entries(&subdir).is_empty());
    assert_eq!(entries(&f.root()).len(), 2);
    assert!(WorktreeStore::from_directory(File::open(f.root().join("A")).unwrap()).is_err());
    let store = f.store();
    let scratch = f.path().join("scratch");
    let pair = snapshot().checkout_variants(&store).unwrap();
    let decision = CandidateDecision::Confirm(pair.choices().next().unwrap().id.clone());
    let window = pair.present(Attention::Verified).unwrap();
    fs::set_permissions(&scratch, fs::Permissions::from_mode(0o770)).unwrap();
    assert!(WorktreeStore::from_directory(File::open(&scratch).unwrap()).is_err());
    assert!(
        window
            .select_checkout(decision, Attention::Verified)
            .is_err()
    );
    assert!(entries(&scratch).is_empty());
}

#[test]
fn named_checkouts_cannot_cross_packed_extents_or_promote_an_edit_to_a_goal() {
    let f = Fixture::new();
    let data = vec![b'a'; MAX_READ_BYTES + 5];
    write(&f.root().join("A"), &data);
    write(&f.root().join("B"), b"short");
    symlink(f.path(), f.root().join("Link")).unwrap();
    let ws = f.workspace(&["A", "B", "Link/outside"]);
    let store = f.store();
    write(&f.path().join("outside"), b"unapproved");
    assert!(checkout_candidates(&objective("read file A or Link/outside"), &ws, &store).is_err());
    assert!(
        capture(&ws, "read file A then read file A")
            .unwrap()
            .checkout_variants(&store)
            .is_err()
    );
    let mut branches = checkout_candidates(&objective("read file A or B"), &ws, &store).unwrap();
    let ids: Vec<_> = branches.choices().map(|c| c.id.clone()).collect();
    assert!(branches.overwrite(&TaskId::new(), 0, b"x").is_err());
    assert!(
        branches
            .overwrite(&ids[0], MAX_READ_BYTES as u64 - 1, b"xx")
            .is_err()
    );
    assert!(branches.overwrite(&ids[1], 5, b"x").is_err());
    let literal = b"then write file B";
    branches.overwrite(&ids[0], 0, literal).unwrap();
    branches.overwrite(&ids[1], 4, &[0xff]).unwrap();
    let selected = branches
        .present(Attention::Verified)
        .unwrap()
        .select_checkout(
            CandidateDecision::Confirm(ids[0].clone()),
            Attention::Verified,
        )
        .unwrap();
    assert_eq!(selected.file_len, data.len());
    let selected_bytes = selected.directory.read_file("A").unwrap();
    assert_eq!(selected_bytes.len(), MAX_READ_BYTES);
    assert_eq!(&selected_bytes[..literal.len()], literal);
    assert_eq!(
        &selected_bytes[literal.len()..],
        &data[literal.len()..MAX_READ_BYTES]
    );
    assert_eq!(fs::read(f.root().join("A")).unwrap(), data);
    assert_eq!(fs::read(f.root().join("B")).unwrap(), b"short");
}

#[test]
fn an_empty_extent_cannot_expose_or_overwrite_the_mmap_sentinel() {
    let f = Fixture::new();
    write(&f.root().join("Empty"), b"");
    let ws = f.workspace(&["Empty"]);
    let mut branches = capture(&ws, "read file Empty")
        .unwrap()
        .checkout_variants(&f.store())
        .unwrap();
    let id = branches.choices().next().unwrap().id.clone();
    assert!(branches.overwrite(&id, 0, b"x").is_err());
    let selected = branches
        .present(Attention::Verified)
        .unwrap()
        .select_checkout(CandidateDecision::Confirm(id), Attention::Verified)
        .unwrap();
    assert!(selected.directory.read_file("Empty").unwrap().is_empty());
    assert_eq!(selected.directory.file_metadata("Empty").unwrap().len(), 0);
    assert!(selected.choice.overwrite.is_none());
    assert_eq!(fs::metadata(f.root().join("Empty")).unwrap().len(), 0);
}

#[test]
fn countermand_foreign_choice_attention_loss_and_expiry_discard_private_edits() {
    let f = Fixture::new();
    write(&f.root().join("A"), b"abc");
    write(&f.root().join("B"), b"def");
    let ws = f.workspace(&["A", "B"]);
    let store = f.store();
    let scratch = f.path().join("scratch");
    let count = || fs::read_dir(&scratch).unwrap().count();
    let stage = || {
        let mut branches =
            checkout_candidates(&objective("read file A or B"), &ws, &store).unwrap();
        let id = branches.choices().last().unwrap().id.clone();
        branches.overwrite(&id, 0, b"XYZ").unwrap();
        assert_eq!(count(), 2);
        assert!(open_children(&scratch) > 0);
        (branches, CandidateDecision::Confirm(id))
    };
    drop(stage());
    assert_eq!(count(), 0);
    assert_eq!(open_children(&scratch), 0);
    assert!(stage().0.present(Attention::Unverified).is_err());
    assert_eq!(count(), 0);
    let window = || {
        let (branches, choice) = stage();
        let window = branches.present(Attention::Verified).unwrap();
        assert_eq!(count(), 0, "unanswered window left staged files");
        assert_eq!(open_children(&scratch), 0);
        (window, choice)
    };
    let (withdrawn, stale) = window();
    withdrawn.cancel();
    assert!(
        window()
            .0
            .select_checkout(stale, Attention::Verified)
            .is_err()
    );
    assert!(
        window()
            .0
            .select_checkout(CandidateDecision::Nack, Attention::Verified)
            .is_err()
    );
    let (lost, id) = window();
    assert!(lost.select_checkout(id, Attention::Unverified).is_err());
    let (pending, id) = window();
    std::thread::sleep(CONFIRMATION_WINDOW);
    assert!(pending.select_checkout(id, Attention::Verified).is_err());
    drop(window());
    assert_eq!(count(), 0);
    assert_eq!(fs::read(f.root().join("A")).unwrap(), b"abc");
    assert_eq!(fs::read(f.root().join("B")).unwrap(), b"def");
    assert_eq!(fs::read_dir(f.root()).unwrap().count(), 2);
}
