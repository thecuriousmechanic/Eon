#[path = "checkouts.rs"]
mod checkouts;
#[path = "fixture.rs"]
mod fixture;
use fixture::{Fixture, mkdir, write};
use std::fs::{self, File, Permissions};
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt, symlink};
use std::path::Path;
use std::time::Duration;

use eon_cognition::compiler::{
    Attention, CONFIRMATION_WINDOW, CandidateDecision, Decision, ReadSnapshot, ReadWorkspace,
    WorktreeStore, checkout_candidates, compile, preview_candidates,
};
use rustix::fs::Mode;

use super::objective;

impl Fixture {
    fn workspace(&self, public: &[&str]) -> ReadWorkspace {
        ReadWorkspace::from_directory(File::open(self.root()).unwrap(), public).unwrap()
    }

    fn store(&self) -> WorktreeStore {
        let scratch = self.path().join("scratch");
        mkdir(&scratch);
        WorktreeStore::from_directory(File::open(scratch).unwrap()).unwrap()
    }
}

fn capture(workspace: &ReadWorkspace, intent: &str) -> eon_cognition::EonResult<ReadSnapshot> {
    compile(&objective(intent))?.snapshot(workspace)
}

#[test]
fn selects_a_pinned_workspace_and_executes_ranges_from_the_isolated_image() {
    let f = Fixture::new();
    mkdir(&f.root().join("src"));
    let name = "src/Public.txt";
    write(&f.root().join(name), b"0123456789");
    let workspace = f.workspace(&[name]);
    // Neither replacing the selected root's name before capture nor changing
    // the original inode after capture redirects the shadow plan.
    let original = f.path().join("moved");
    fs::rename(f.root(), &original).unwrap();
    mkdir(&f.root());
    mkdir(&f.root().join("src"));
    write(&f.root().join(name), b"elsewhere!");
    let snapshot = capture(&workspace, "read 4 bytes from src/Public.txt at offset 3 then read 4 bytes from src/Public.txt at offset 9 then read 1 bytes from src/Public.txt at offset 10").unwrap();
    drop(workspace);
    fs::write(original.join(name), b"changed").unwrap();
    let staged = snapshot.preview();
    assert_eq!(staged.requests().count(), 3);
    let (workflow, reads) = staged
        .present(Attention::Verified)
        .unwrap()
        .respond(Decision::Confirm, Attention::Verified)
        .unwrap()
        .into_parts();
    assert_eq!(
        workflow.validate_topological_order().unwrap(),
        reads
            .iter()
            .map(|r| r.request.task_id.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        reads.iter().map(|r| &*r.bytes).collect::<Vec<_>>(),
        [b"3456".as_slice(), b"9", b""]
    );
    assert!(reads.iter().all(|r| r.file_len == 10));
    assert_eq!(fs::read(original.join(name)).unwrap(), b"changed");
    assert_eq!(fs::read(f.root().join(name)).unwrap(), b"elsewhere!");
}

#[test]
fn only_requested_public_extents_are_captured_including_sparse_and_empty_files() {
    let f = Fixture::new();
    let root = f.root();
    write(&root.join("Empty"), b"");
    write(&root.join("Large"), b"");
    const OFFSET: u64 = 1 << 30;
    File::options()
        .write(true)
        .open(root.join("Large"))
        .unwrap()
        .write_all_at(b"tail", OFFSET)
        .unwrap();
    mkdir(&root.join(".git"));
    write(&root.join(".git/config"), b"not public");
    write(&root.join(".env"), b"not public");
    write(&root.join("Other.txt"), b"not approved");
    let workspace = f.workspace(&["Large", "Empty", "Missing"]);
    for denied in ["Other.txt", "Missing"] {
        assert!(
            capture(
                &workspace,
                &format!("read file Large then read file {denied}")
            )
            .is_err()
        );
    }
    assert!(
        capture(
            &workspace,
            "read 1 bytes from Large at offset 18446744073709551615"
        )
        .is_err()
    );
    for name in [
        ".git/config",
        "private.git/config",
        ".env",
        "src/Credentials.json",
        "id_ed25519",
        "nested/tls.KEY",
        "secrets/private.txt",
    ] {
        assert!(ReadWorkspace::from_directory(File::open(&root).unwrap(), &[name]).is_err());
        assert!(compile(&objective(&format!("read file {name}"))).is_err());
    }
    for (intent, expected, length) in [
        ("read file Empty".to_owned(), b"".as_slice(), 0),
        (
            format!("read 2 bytes from Large at offset {OFFSET}"),
            b"ta",
            OFFSET as usize + 4,
        ),
    ] {
        let (_, reads) = capture(&workspace, &intent)
            .unwrap()
            .preview()
            .present(Attention::Verified)
            .unwrap()
            .respond(Decision::Confirm, Attention::Verified)
            .unwrap()
            .into_parts();
        assert_eq!(reads[0].bytes, expected);
        assert_eq!(reads[0].file_len, length);
    }
}

#[test]
fn refuses_leaf_and_ancestor_links_shared_files_directories_and_special_files() {
    let f = Fixture::new();
    let root = f.root();
    let outside = f.path().join("outside");
    mkdir(&outside);
    write(&outside.join("Data"), b"outside");
    symlink(outside.join("Data"), root.join("Link")).unwrap();
    symlink(&outside, root.join("DirLink")).unwrap();
    fs::hard_link(outside.join("Data"), root.join("Hard")).unwrap();
    mkdir(&root.join("Shared"));
    write(&root.join("Shared/File"), b"file");
    fs::set_permissions(root.join("Shared"), Permissions::from_mode(0o770)).unwrap();
    write(&root.join("Writable"), b"file");
    fs::set_permissions(root.join("Writable"), Permissions::from_mode(0o620)).unwrap();
    rustix::fs::mkfifoat(&f.dir, "fixture/workspace/Pipe", Mode::RUSR).unwrap();
    let cases = [
        "Link",
        "DirLink/Data",
        "Hard",
        "Shared/File",
        "Writable",
        "Pipe",
        "Shared",
    ];
    let ws = f.workspace(&cases);
    for name in cases {
        assert!(
            capture(&ws, &format!("read file {name}")).is_err(),
            "{name}"
        );
    }
    assert_eq!(fs::read(outside.join("Data")).unwrap(), b"outside");
}

#[test]
fn path_constructor_never_downgrades_untrusted_ancestry_to_a_trusted_handle() {
    let f = Fixture::new();
    let real = fs::canonicalize(f.root()).unwrap();
    let uid = rustix::process::geteuid().as_raw();
    let safe_ancestry = real.ancestors().all(|dir| {
        let m = dir.metadata().unwrap();
        (m.uid() == uid || m.uid() == 0) && m.mode() & 0o022 == 0
    });
    write(&real.join("File"), b"public");
    match ReadWorkspace::open(&real, &["File"]) {
        Ok(ws) => {
            assert!(safe_ancestry);
            assert!(capture(&ws, "read file File").is_ok());
        }
        Err(e) => assert!(!safe_ancestry, "{e}"),
    }
    assert!(ReadWorkspace::open(Path::new("workspace"), &[]).is_err());
    let alias = real.with_file_name("link");
    symlink(&real, &alias).unwrap();
    assert!(ReadWorkspace::open(&alias, &[]).is_err());
    symlink(real.parent().unwrap(), real.with_file_name("parent-link")).unwrap();
    assert!(ReadWorkspace::open(&real.with_file_name("parent-link/workspace"), &[]).is_err());
    fs::set_permissions(real.parent().unwrap(), Permissions::from_mode(0o770)).unwrap();
    assert!(ReadWorkspace::open(&real, &[]).is_err());
    fs::set_permissions(&real, Permissions::from_mode(0o770)).unwrap();
    assert!(ReadWorkspace::from_directory(File::open(&real).unwrap(), &[]).is_err());
}

#[test]
fn filesystem_preview_nack_cancel_attention_loss_and_real_expiry_release_nothing() {
    let f = Fixture::new();
    write(&f.root().join("File"), b"only evidence");
    let ws = f.workspace(&["File"]);
    let staged = || capture(&ws, "read file File").unwrap().preview();
    assert!(staged().present(Attention::Unverified).is_err());
    let window = || staged().present(Attention::Verified).unwrap();
    window().cancel();
    assert!(
        window()
            .respond(Decision::Deny, Attention::Verified)
            .is_err()
    );
    assert!(
        window()
            .respond(Decision::Confirm, Attention::Unverified)
            .is_err()
    );
    let pending = window();
    assert_eq!(CONFIRMATION_WINDOW, Duration::from_secs(5));
    std::thread::sleep(CONFIRMATION_WINDOW);
    assert!(
        pending
            .respond(Decision::Confirm, Attention::Verified)
            .is_err()
    );
    assert_eq!(fs::read(f.root().join("File")).unwrap(), b"only evidence");
    assert_eq!(fs::read_dir(f.root()).unwrap().count(), 1);
}

#[test]
fn structured_alternatives_release_only_the_chosen_snapshot_and_validated_dag() {
    let f = Fixture::new();
    write(&f.root().join("A"), b"first");
    write(&f.root().join("B"), b"second");
    let ws = f.workspace(&["A", "B"]);
    let objective = objective("Read file A OR B");
    assert!(compile(&objective).is_err());
    let candidates = preview_candidates(&objective, &ws, &f.store()).unwrap();
    let scratch = f.path().join("scratch");
    assert_eq!(fs::read_dir(&scratch).unwrap().count(), 2);
    let choices: Vec<_> = candidates.requests().collect();
    assert_eq!(
        choices.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
        ["A", "B"]
    );
    let id = choices[1].task_id.clone();
    assert_ne!(id, choices[0].task_id);
    fs::remove_file(f.root().join("A")).unwrap();
    fs::write(f.root().join("B"), b"changed").unwrap();
    let window = candidates.present(Attention::Verified).unwrap();
    assert_eq!(fs::read_dir(&scratch).unwrap().count(), 0);
    let (workflow, reads) = window
        .select(CandidateDecision::Confirm(id.clone()), Attention::Verified)
        .unwrap()
        .into_parts();
    assert_eq!(workflow.objective_id(), &objective.id);
    assert_eq!(
        workflow.validate_topological_order().unwrap(),
        std::slice::from_ref(&id)
    );
    assert_eq!(reads.len(), 1);
    assert_eq!(reads[0].request.task_id, id);
    assert_eq!(reads[0].bytes, b"second");
    assert_eq!(fs::read(f.root().join("B")).unwrap(), b"changed");
}

#[test]
fn ambiguous_candidate_grammar_and_capture_fail_as_a_unit() {
    let f = Fixture::new();
    write(&f.root().join("A"), b"a");
    write(&f.root().join("B"), b"b");
    write(&f.root().join("Private"), b"not approved");
    let ws = f.workspace(&["A", "B", "Missing"]);
    let store = f.store();
    for intent in [
        "read whatever looks useful",
        "read file A",
        "read file A or A",
        "read file A or B or C",
        "read file A or B then read file A",
        "read file A or ../B",
        "read file A or Missing",
        "read file A or Private",
        "read file A or .git/config",
        "read file A or\nB",
        "read file A or B;run",
        "write file A or B",
        "read file A or read file A",
        "read file A or read file Missing",
        "read file A or read file B then read file A",
        "read file A then read file B or read file A",
        "read file A or read file B or read file A",
    ] {
        assert!(
            preview_candidates(&objective(intent), &ws, &store).is_err(),
            "{intent}"
        );
        assert!(
            checkout_candidates(&objective(intent), &ws, &store).is_err(),
            "{intent}"
        );
    }
}

#[test]
fn candidate_presentation_is_one_window_not_refreshable_on_selection() {
    let f = Fixture::new();
    write(&f.root().join("A"), b"a");
    write(&f.root().join("B"), b"b");
    let ws = f.workspace(&["A", "B"]);
    let store = f.store();
    let choices = || preview_candidates(&objective("read file A or B"), &ws, &store).unwrap();
    assert!(choices().present(Attention::Unverified).is_err());
    let window = || {
        let pair = choices();
        let choice = CandidateDecision::Confirm(pair.requests().next().unwrap().task_id.clone());
        (pair.present(Attention::Verified).unwrap(), choice)
    };
    let (first, stale) = window();
    first.cancel();
    assert!(window().0.select(stale, Attention::Verified).is_err());
    assert!(
        window()
            .0
            .select(CandidateDecision::Nack, Attention::Verified)
            .is_err()
    );
    let (pending, choice) = window();
    assert!(pending.select(choice, Attention::Unverified).is_err());
    let (pending, choice) = window();
    std::thread::sleep(CONFIRMATION_WINDOW);
    assert!(pending.select(choice, Attention::Verified).is_err());
    assert_eq!(fs::read_dir(f.root()).unwrap().count(), 2);
}
