//! Opt-in production stdio test: supply the absolute path to a trusted native
//! clangd in EON_TEST_CLANGD and its read-only runtime paths (colon-separated)
//! in EON_TEST_CLANGD_RUNTIME. No PATH discovery, project build or shell.
#![cfg(target_os = "linux")]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eon_capability::local_lsp::{LocalLspPool, OperatorServer, Position, ServerKind, preview_replacement};
use eon_capability::workspace::Workspace;

struct Project(PathBuf);

impl Project {
    fn new() -> Self {
        let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
        fs::create_dir_all(&parent).unwrap();
        let root = parent
            .canonicalize()
            .unwrap()
            .join(format!("eon-clangd-{:016x}", getrandom::u64().unwrap()));
        fs::create_dir(&root).unwrap();
        Self(root)
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn lsp_and_patcher_tests_digest_bound_preview_on_local_project() {
    let p = Project::new();
    let workspace = Workspace::open(&p.0).unwrap();
    for (path, prefix, function, suffix, replacement) in [
        (
            "lib.rs",
            "// fn score() {} 🦀\nmod nested { fn score() {} }\n#[inline]\n",
            "pub fn score( n: i32 ) -> i32 {\n    n * 2\n}",
            "\nfn keep() {}\n",
            "pub fn score(n: i32) -> i32 { n + 2 }",
        ),
        (
            "model.py",
            "# def score(n):\nclass Model:\n    def score(self): return 4\n\n",
            "def score( n ):\n    return 'λ' * n",
            "\n\ndef keep(): pass\n",
            "def score(n):\n    return 'λ' * (n + 1)",
        ),
    ] {
        let original = format!("{prefix}{function}{suffix}");
        fs::write(p.0.join(path), &original).unwrap();
        let preview = preview_replacement(&workspace, path, "score", replacement).unwrap();
        assert_eq!(preview.relative_path, path);
        assert_eq!(preview.before, original);
        assert_eq!(preview.after, format!("{prefix}{replacement}{suffix}"));
        let read = workspace.read_range(path, 0, original.len()).unwrap();
        assert_eq!(read.sha256, preview.expected_sha256);
        assert_eq!(read.bytes, original.as_bytes());
    }
}

fn sample(label: &str, mut query: impl FnMut(usize)) {
    let mut samples = Vec::new();
    for i in 0..20 {
        let start = Instant::now();
        query(i);
        samples.push(start.elapsed());
    }
    samples.sort_unstable();
    eprintln!("{label} (20 samples): p95={:?}, max={:?}", samples[18], samples[19]);
}

#[test]
#[ignore = "requires an operator-provisioned clangd, runtime and Linux worker confinement"]
fn lsp_and_patcher_tests_confined_clangd_queries() {
    let exe = std::env::var_os("EON_TEST_CLANGD").expect("set EON_TEST_CLANGD");
    let libs = std::env::var_os("EON_TEST_CLANGD_RUNTIME").expect("set EON_TEST_CLANGD_RUNTIME");
    let cfg = OperatorServer::new(
        ServerKind::Clangd,
        Path::new(&exe),
        &std::env::split_paths(&libs).collect::<Vec<_>>(),
        Duration::from_secs(15),
    )
    .unwrap();
    let p = Project::new();
    let implementation = "int twice(int x) { return x * 2; }\n";
    let provider = "twice helper.c";
    let source = "int twice(int);\nint answer(void) { return twice(21); }\nint bad = unknown_name;\n";
    let path = "tiny.c";
    fs::write(p.0.join(provider), implementation).unwrap();
    fs::write(p.0.join(path), source).unwrap();
    // Were workspace configuration discovered, it would mask the error.
    let config = "CompileFlags:\n  Add: [-Dunknown_name=42]\n";
    fs::write(p.0.join(".clangd"), config).unwrap();

    let start = Instant::now();
    let mut pool = LocalLspPool::start(&[cfg], &p.0).unwrap();
    eprintln!("initialize: {:?}", start.elapsed());
    let server = pool.session(ServerKind::Clangd).unwrap();
    let start = Instant::now();
    server.open(provider).unwrap();
    server.open(path).unwrap();
    let diagnostics = server.diagnostics(path).unwrap();
    assert!(
        diagnostics.iter().any(|d| d.range.start.line == 2 && d.message.contains("unknown_name")),
        "{diagnostics:?}"
    );
    eprintln!("open both documents to first diagnostic: {:?}", start.elapsed());
    let pos = Position { line: 1, character: 28 };
    sample("hover", |_| {
        let hover = server.hover_type(path, pos).unwrap().unwrap();
        assert!(hover.contains("twice") && hover.contains("int"), "{hover:?}");
    });
    sample("definition", |_| {
        let defs = server.goto_definition(path, pos).unwrap();
        assert!(
            defs.iter().any(|d| d.relative_path == provider && d.range.start.line == 0),
            "{defs:?}"
        );
    });
    sample("references", |_| {
        let refs = server.find_references(path, pos).unwrap();
        assert!(
            refs.iter().any(|r| r.relative_path == provider && r.range.start.line == 0),
            "{refs:?}"
        );
        assert!(
            refs.iter().any(|r| r.relative_path == path && r.range.start.line == 1),
            "{refs:?}"
        );
    });
    sample("workspace symbols", |_| {
        let symbols = server.workspace_symbols("twice").unwrap();
        assert!(symbols.iter().any(|s| s.name == "twice" && s.location.relative_path == provider));
    });
    sample("cached push diagnostics", |_| {
        assert_eq!(server.diagnostics(path).unwrap(), diagnostics);
    });
    let fixed = source.replace("unknown_name", "42");
    sample("didChange to new push diagnostics", |i| {
        server.change(path, if i % 2 == 0 { &fixed } else { source }).unwrap();
        let items = server.diagnostics(path).unwrap();
        if i % 2 == 0 {
            assert!(items.is_empty(), "{items:?}");
        } else {
            assert_eq!(items, diagnostics);
        }
    });
    server.change(path, "").unwrap();
    assert!(server.diagnostics(path).unwrap().is_empty());
    server.close(path).unwrap();
    assert!(server.goto_definition(path, pos).is_err());
    server.open(path).unwrap(); // a new snapshot from disk, not the empty overlay
    assert_eq!(server.diagnostics(path).unwrap(), diagnostics);
    server.close(path).unwrap();
    server.close(provider).unwrap();
    // clangd's post-exit watchdog requires a thread, not admitted by the
    // native worker ABI. Exercise forced cleanup, not a claim of clean exit.
    drop(pool);
    assert_eq!(fs::read_to_string(p.0.join(path)).unwrap(), source);
    assert_eq!(fs::read_to_string(p.0.join(provider)).unwrap(), implementation);
    assert_eq!(fs::read_to_string(p.0.join(".clangd")).unwrap(), config);
    assert_eq!(fs::read_dir(&p.0).unwrap().count(), 3);
}
