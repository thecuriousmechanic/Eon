#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Instant;

use eon_capability::local_lsp::preview_replacement;
use eon_capability::workspace::{Workspace, WorkspaceError};
use eon_foundation::group_commit::GroupCommitJournal;
use eon_foundation::owned_dir::OwnedDirectory;

struct Project(PathBuf);

impl Drop for Project {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn private_dir(path: &Path) {
    fs::DirBuilder::new().mode(0o700).create(path).unwrap();
}

#[test]
fn lsp_and_patcher_tests_apply_reviewed_edits_in_a_mixed_frontend_project() {
    // Both the journal and the disposable fixture copy need owned ancestry.
    let home = std::env::home_dir().unwrap();
    let parent = OwnedDirectory::open(&home, false).unwrap();
    let name = format!(".eon-ast-project-{:016x}", getrandom::u64().unwrap());
    private_dir(&parent.path().join(&name));
    let project = Project(home.join(name));
    let root = project.0.join("project");
    private_dir(&root);
    private_dir(&root.join("src"));
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/local_lsp_project");
    let files = [
        "package.json",
        "src/money.js",
        "src/quote.ts",
        "src/Amount.jsx",
        "src/QuoteCard.tsx",
    ];
    for path in files {
        fs::copy(fixture.join(path), root.join(path)).unwrap();
        fs::set_permissions(root.join(path), fs::Permissions::from_mode(0o600)).unwrap();
    }

    let workspace = Workspace::open(&root).unwrap();
    let journal = GroupCommitJournal::spawn(project.0.join("journal"), 16).unwrap();
    for (path, symbol, old_body, new_body) in [
        (
            "src/money.js",
            "subtotal",
            "return lines.reduce((sum, { cents, count }) => sum + cents * count, 0);",
            "return lines.reduce((sum, { cents, count }) => sum + cents * Math.max(0, count), 0);",
        ),
        (
            "src/quote.ts",
            "quote",
            "return subtotal(lines);",
            "return Math.max(0, subtotal(lines));",
        ),
        (
            "src/Amount.jsx",
            "Amount",
            "return <span>{display(cents)}</span>;",
            "return <output>{display(cents)}</output>;",
        ),
        (
            "src/QuoteCard.tsx",
            "QuoteCard",
            "return <article><Amount cents={quote(lines)} /></article>;",
            "return <section><Amount cents={quote(lines)} /></section>;",
        ),
    ] {
        let original = fs::read_to_string(fixture.join(path)).unwrap();
        let expected = original.replace(old_body, new_body);
        assert_eq!(original.matches(old_body).count(), 1);
        let declaration_start = original.find(&format!("function {symbol}")).unwrap();
        let header = original[declaration_start..].lines().next().unwrap();
        let replacement = format!("{header}\n    {new_body}\n}}");
        let preview = || preview_replacement(&workspace, path, symbol, &replacement).unwrap();
        let start = Instant::now();
        let proposal = preview();
        let first = start.elapsed();
        assert_eq!(proposal.relative_path, path);
        assert_eq!(proposal.before, original);
        assert_eq!(proposal.after, expected);
        let mut warm = [std::time::Duration::ZERO; 20];
        for sample in &mut warm {
            let start = Instant::now();
            let repeated = preview();
            *sample = start.elapsed();
            assert_eq!(repeated.after, expected);
        }
        warm.sort_unstable();
        eprintln!(
            "{path} structural preview: first={first:?}, warm p95 (20)={:?}",
            warm[18]
        );
        assert_eq!(fs::read_to_string(root.join(path)).unwrap(), original);

        // Explicit fixture approval is separate from preview. The exact diff
        // was checked above; do not generate or run a project build.
        let apply = || workspace.edit(path, proposal.expected_sha256, proposal.after.as_bytes(), &journal);
        apply().unwrap();
        assert!(matches!(apply(), Err(WorkspaceError::Conflict)));
        assert_eq!(fs::read_to_string(root.join(path)).unwrap(), expected);
    }
    assert_eq!(
        fs::read(root.join("package.json")).unwrap(),
        fs::read(fixture.join("package.json")).unwrap()
    );
}
