//! Real server gate; see “Provisioning the confined Rust LSP” in the capability fabric specification.
#![cfg(all(
    target_os = "linux",
    target_arch = "x86_64",
    target_env = "gnu",
    target_pointer_width = "64"
))]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eon_capability::local_lsp::{Diagnostic, LocalLspPool, OperatorServer, Position, ServerKind};

struct Project(PathBuf);

impl Drop for Project {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn benchmark(label: &str, mut query: impl FnMut(usize)) {
    let start = Instant::now();
    query(0);
    let first = start.elapsed();
    let mut warm = [Duration::ZERO; 20];
    for (i, sample) in warm.iter_mut().enumerate() {
        let start = Instant::now();
        query(i + 1);
        *sample = start.elapsed();
    }
    warm.sort_unstable();
    eprintln!(
        "{label}: first in initialized session={first:?}, warm p95 (20)={:?}",
        warm[18]
    );
}

#[test]
#[ignore = "requires a trusted native rust-analyzer, its libraries and Linux confinement"]
fn lsp_and_patcher_tests_confined_rust_analyzer() {
    let exe = std::env::var_os("EON_TEST_RUST_ANALYZER").unwrap();
    let libs = std::env::var_os("EON_TEST_RUST_ANALYZER_RUNTIME").unwrap();
    let group = match std::env::var("EON_TEST_RUST_ANALYZER_CGROUP")
        .unwrap()
        .as_str()
    {
        "self" => Path::new("/sys/fs/cgroup").join(
            fs::read_to_string("/proc/self/cgroup")
                .unwrap()
                .trim()
                .strip_prefix("0::/")
                .unwrap(),
        ),
        path => PathBuf::from(path),
    };
    let cfg = OperatorServer::new(
        ServerKind::RustAnalyzer,
        Path::new(&exe),
        &std::env::split_paths(&libs).collect::<Vec<_>>(),
        Duration::from_secs(8),
    )
    .unwrap()
    .with_rust_analyzer_task_group(&group)
    .unwrap();
    let parent = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target")
        .canonicalize()
        .unwrap();
    let p = Project(parent.join(format!("eon-ra-{:016x}", getrandom::u64().unwrap())));
    fs::create_dir(&p.0).unwrap();
    fs::create_dir(p.0.join("src")).unwrap();
    let lib = "src/lib.rs";
    let math = "src/math.rs";
    let source = "mod math;\npub fn answer() -> i32 { math::twice(21) }\nmod absent;\n";
    let files = [
        (lib, source),
        (math, "pub fn twice(x: i32) -> i32 { x * 2 }\n"),
        // Exists in the real project, not in the authorized overlays.
        ("src/absent.rs", "pub struct NotShared;\n"),
        (
            "Cargo.toml",
            "[package]\nname='tiny'\nversion='0.1.0'\nedition='2021'\n",
        ),
        ("build.rs", "compile_error!(\"must never be built\");\n"),
        ("rust-analyzer.toml", "[diagnostics]\nenable = false\n"),
    ];
    for (path, text) in files {
        fs::write(p.0.join(path), text).unwrap();
    }
    let expected_error = |d: &Diagnostic| {
        d.range.start
            == Position {
                line: 2,
                character: 0,
            }
            && d.range.end
                == Position {
                    line: 2,
                    character: 11,
                }
            && d.message.contains("unresolved module")
            && d.message.contains("absent")
    };
    let start = Instant::now();
    let mut pool = LocalLspPool::start(&[cfg], &p.0).unwrap();
    eprintln!("launch to initialized + model ready: {:?}", start.elapsed());
    let server = pool.session(ServerKind::RustAnalyzer).unwrap();
    server.open(math).unwrap();
    server.open(lib).unwrap();
    let diag = loop {
        let diag = server.diagnostics(lib).unwrap();
        if diag.iter().any(expected_error) {
            break diag;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "missing initial module diagnostic: {diag:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    eprintln!(
        "cold launch to first diagnostics: {:?}: {diag:?}",
        start.elapsed()
    );
    let pos = Position {
        line: 1,
        character: 33,
    };
    let declaration = Position {
        line: 0,
        character: 7,
    };
    let usage = Position {
        line: 1,
        character: 31,
    };
    benchmark("definition", |_| {
        let defs = server.goto_definition(lib, pos).unwrap();
        assert_eq!(defs.len(), 1, "{defs:?}");
        assert_eq!(
            (&*defs[0].relative_path, defs[0].range.start),
            (math, declaration)
        );
    });
    benchmark("hover", |_| {
        let hover = server.hover_type(lib, pos).unwrap().unwrap();
        assert!(hover.contains("fn twice(x: i32) -> i32"), "{hover}");
    });
    benchmark("references", |_| {
        let refs = server.find_references(lib, pos).unwrap();
        assert_eq!(refs.len(), 2, "{refs:?}");
        for (path, start) in [(lib, usage), (math, declaration)] {
            assert!(
                refs.iter()
                    .any(|r| r.relative_path == path && r.range.start == start),
                "{refs:?}"
            );
        }
    });
    benchmark("pull diagnostics", |_| {
        assert!(server.diagnostics(lib).unwrap().iter().any(expected_error));
    });
    let fixed = source.replace("mod absent;\n", "");
    benchmark("change to fresh pull diagnostics", |i| {
        server
            .change(lib, if i % 2 == 0 { &fixed } else { source })
            .unwrap();
        let items = server.diagnostics(lib).unwrap();
        if i % 2 == 0 {
            assert!(items.is_empty(), "{items:?}");
        } else {
            assert!(items.iter().any(expected_error), "{items:?}");
        }
    });
    pool.shutdown().unwrap();
    for (path, text) in files {
        assert_eq!(fs::read_to_string(p.0.join(path)).unwrap(), text);
    }
    assert_eq!(fs::read_dir(&p.0).unwrap().count(), 4);
    assert_eq!(fs::read_dir(p.0.join("src")).unwrap().count(), 3);
}
