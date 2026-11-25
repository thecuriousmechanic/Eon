#![cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]

use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eon_capability::local_lsp::{LocalLspPool, OperatorServer, Position, ServerKind};

struct Project(PathBuf);

impl Drop for Project {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn sample(label: &str, mut run: impl FnMut(usize)) {
    let mut times = [Duration::ZERO; 20];
    for (n, duration) in times.iter_mut().enumerate() {
        let start = Instant::now();
        run(n);
        *duration = start.elapsed();
    }
    times.sort_unstable();
    eprintln!("{label} p95/20: {:?}", times[18]);
}

#[test]
#[ignore = "requires pinned Pyright SEA, trusted runtime and capped cgroup v2"]
fn lsp_and_patcher_tests_confined_pyright() {
    let exe = PathBuf::from(std::env::var_os("EON_TEST_PYRIGHT").unwrap())
        .canonicalize()
        .unwrap();
    let libs = std::env::var_os("EON_TEST_PYRIGHT_RUNTIME").unwrap();
    let mine = fs::read_to_string("/proc/self/cgroup").unwrap();
    let group = Path::new("/sys/fs/cgroup").join(mine.trim().strip_prefix("0::/").unwrap());
    let pin = std::env::var("EON_TEST_PYRIGHT_SHA256").unwrap();
    let pin: [u8; 32] =
        std::array::from_fn(|i| u8::from_str_radix(&pin[i * 2..i * 2 + 2], 16).unwrap());
    let cfg = OperatorServer::new(
        ServerKind::Pyright,
        &exe,
        &std::env::split_paths(&libs).collect::<Vec<_>>(),
        Duration::from_secs(8),
    )
    .unwrap()
    .with_pyright_task_group(&group, &std::env::temp_dir(), pin)
    .unwrap();
    let project = Project(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .canonicalize()
            .unwrap()
            .join(format!("eon-pyright-{:016x}", getrandom::u64().unwrap())),
    );
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&project.0)
        .unwrap();
    let source = "from money import twice\namount: int = twice('bad')\n";
    let fixed = source.replace("'bad'", "3");
    let provider = "def twice(value: int) -> int:\n    return value * 2\n";
    fs::write(project.0.join("money.py"), provider).unwrap();
    fs::write(project.0.join("quote.py"), source).unwrap();
    fs::write(
        project.0.join("pyrightconfig.json"),
        r#"{"typeCheckingMode":"off"}"#,
    )
    .unwrap();
    let start = Instant::now();
    let mut pool = LocalLspPool::start(&[cfg], &project.0).unwrap();
    let server = pool.session(ServerKind::Pyright).unwrap();
    server.open("money.py").unwrap();
    server.open("quote.py").unwrap();
    let diagnostic = server.diagnostics("quote.py").unwrap();
    assert!(
        diagnostic.iter().any(|d| d.range.start
            == (Position {
                line: 1,
                character: 20
            })
            && d.message.contains("int")
            && d.message.contains("Literal['bad']")
            && d.message.contains("twice")),
        "{diagnostic:?}"
    );
    eprintln!("launch to first semantic diagnostic: {:?}", start.elapsed());
    let pos = Position {
        line: 1,
        character: 15,
    };
    sample("cross-file definition", |_| {
        let defs = server.goto_definition("quote.py", pos).unwrap();
        assert!(
            defs.iter()
                .any(|d| d.relative_path == "money.py" && d.range.start.line == 0),
            "{defs:?}"
        );
    });
    sample("references", |_| {
        let refs = server.find_references("quote.py", pos).unwrap();
        for (file, line) in [("money.py", 0), ("quote.py", 1)] {
            assert!(
                refs.iter()
                    .any(|d| d.relative_path == file && d.range.start.line == line),
                "{refs:?}"
            );
        }
    });
    sample("hover", |_| {
        let hover = server.hover_type("quote.py", pos).unwrap().unwrap();
        assert!(
            hover.contains("twice") && hover.contains("value: int") && hover.contains("-> int"),
            "{hover}"
        );
    });
    sample("warm diagnostic", |_| {
        assert_eq!(server.diagnostics("quote.py").unwrap(), diagnostic)
    });
    sample("edit to fresh diagnostic", |n| {
        let repaired = n % 2 == 0;
        server
            .change("quote.py", if repaired { &fixed } else { source })
            .unwrap();
        let d = server.diagnostics("quote.py").unwrap();
        if repaired {
            assert!(d.is_empty(), "{d:?}")
        } else {
            assert_eq!(d, diagnostic)
        }
    });
    server.close("money.py").unwrap();
    let missing = server.diagnostics("quote.py").unwrap();
    assert!(
        missing
            .iter()
            .any(|d| d.range.start.line == 0 && d.message.contains("could not be resolved")),
        "{missing:?}"
    );
    server.open("money.py").unwrap();
    assert_eq!(server.diagnostics("quote.py").unwrap(), diagnostic);
    pool.shutdown().unwrap();
    assert_eq!(
        fs::read_to_string(project.0.join("quote.py")).unwrap(),
        source
    );
}
