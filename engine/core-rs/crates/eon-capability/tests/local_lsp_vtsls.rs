#![cfg(all(
    target_os = "linux",
    target_arch = "x86_64",
    target_env = "gnu",
    target_pointer_width = "64"
))]

use eon_capability::local_lsp::{LocalLspPool, OperatorServer, Position, ServerKind};
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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
#[ignore = "requires pinned vtsls SEA, approved libraries and a capped cgroup v2"]
fn confined_vtsls_open_snapshots() {
    let image = PathBuf::from(std::env::var_os("EON_TEST_VTSLS").unwrap());
    let pin = std::env::var("EON_TEST_VTSLS_SHA256").unwrap();
    assert_eq!(pin.len(), 64);
    let pin = std::array::from_fn(|i| u8::from_str_radix(&pin[i * 2..i * 2 + 2], 16).unwrap());
    let mine = fs::read_to_string("/proc/self/cgroup").unwrap();
    let group = Path::new("/sys/fs/cgroup").join(mine.trim().strip_prefix("0::/").unwrap());
    let libs = std::env::var_os("EON_TEST_VTSLS_RUNTIME").unwrap();
    let cfg = OperatorServer::new(
        ServerKind::Vtsls,
        &image,
        &std::env::split_paths(&libs).collect::<Vec<_>>(),
        Duration::from_secs(8),
    )
    .unwrap()
    .with_vtsls_task_group(&group, pin)
    .unwrap();
    let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
    fs::create_dir_all(&target).unwrap();
    let project = Project(
        target
            .canonicalize()
            .unwrap()
            .join(format!("eon-vtsls-{:016x}", getrandom::u64().unwrap())),
    );
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&project.0)
        .unwrap();
    fs::create_dir(project.0.join("src")).unwrap();
    let source = "import { twice } from './money';\nexport const amount = twice('bad');\n";
    let fixed = source.replace("'bad'", "3");
    let provider = "export function twice(value: number): number { return value * 2; }\n";
    fs::write(project.0.join("src/money.ts"), provider).unwrap();
    fs::write(project.0.join("src/quote.ts"), source).unwrap();
    fs::write(
        project.0.join("tsconfig.json"),
        r#"{"compilerOptions":{"noCheck":true}}"#,
    )
    .unwrap();
    let start = Instant::now();
    let mut pool = LocalLspPool::start(&[cfg], &project.0).unwrap();
    let server = pool.session(ServerKind::Vtsls).unwrap();
    server.open("src/money.ts").unwrap();
    server.open("src/quote.ts").unwrap();
    let d = server.diagnostics("src/quote.ts").unwrap();
    assert!(
        d.iter().any(|e| e.range.start
            == (Position {
                line: 1,
                character: 28
            })
            && e.message.contains("string")
            && e.message.contains("number")),
        "{d:?}"
    );
    eprintln!("launch to first semantic diagnostic: {:?}", start.elapsed());
    let pos = Position {
        line: 1,
        character: 23,
    };
    sample("definition", |_| {
        let locations = server.goto_definition("src/quote.ts", pos).unwrap();
        assert!(
            locations
                .iter()
                .any(|l| l.relative_path == "src/money.ts" && l.range.start.line == 0),
            "{locations:?}"
        );
    });
    sample("references", |_| {
        let locations = server.find_references("src/quote.ts", pos).unwrap();
        for (path, line) in [("src/money.ts", 0), ("src/quote.ts", 1)] {
            assert!(
                locations
                    .iter()
                    .any(|l| l.relative_path == path && l.range.start.line == line),
                "{locations:?}"
            );
        }
    });
    sample("hover", |_| {
        let h = server.hover_type("src/quote.ts", pos).unwrap().unwrap();
        assert!(
            h.contains("twice") && h.contains("value: number") && h.contains("): number"),
            "{h}"
        );
    });
    sample("warm diagnostic", |_| {
        assert_eq!(server.diagnostics("src/quote.ts").unwrap(), d)
    });
    sample("edit to fresh diagnostic", |n| {
        server
            .change("src/quote.ts", if n % 2 == 0 { &fixed } else { source })
            .unwrap();
        let current = server.diagnostics("src/quote.ts").unwrap();
        if n % 2 == 0 {
            assert!(current.is_empty(), "{current:?}")
        } else {
            assert_eq!(current, d)
        }
    });
    server.close("src/money.ts").unwrap();
    let missing = server.diagnostics("src/quote.ts").unwrap();
    assert!(
        missing
            .iter()
            .any(|e| e.range.start.line == 0 && e.message.contains("Cannot find module")),
        "{missing:?}"
    );
    server.open("src/money.ts").unwrap();
    assert_eq!(server.diagnostics("src/quote.ts").unwrap(), d);
    // Changing only the provider must invalidate the consumer's type error.
    server
        .change(
            "src/money.ts",
            "export function twice(value: string) { return value.length; }\n",
        )
        .unwrap();
    assert!(server.diagnostics("src/quote.ts").unwrap().is_empty());
    server
        .change("src/quote.ts", "export const broken = ;\n")
        .unwrap();
    let syntax = server.diagnostics("src/quote.ts").unwrap();
    assert!(
        syntax
            .iter()
            .any(|e| e.message.contains("Expression expected")),
        "{syntax:?}"
    );

    fs::write(
        project.0.join("src/format.js"),
        "/** @param {number} value */\nexport const label = value => value.toFixed(2);\n",
    )
    .unwrap();
    fs::write(
        project.0.join("src/Badge.jsx"),
        "import { label } from './format';\nexport const Badge = () => <b>{label(2)}</b>;\n",
    )
    .unwrap();
    fs::write(
        project.0.join("src/Card.tsx"),
        "import { Badge } from './Badge';\nexport const card = <Badge />;\n",
    )
    .unwrap();
    for path in ["src/format.js", "src/Badge.jsx", "src/Card.tsx"] {
        server.open(path).unwrap();
    }
    for (from, character, to) in [
        ("src/Badge.jsx", 33, "src/format.js"),
        ("src/Card.tsx", 23, "src/Badge.jsx"),
    ] {
        let locations = server
            .goto_definition(from, Position { line: 1, character })
            .unwrap();
        assert!(
            locations
                .iter()
                .any(|l| l.relative_path == to && l.range.start.line == 1),
            "{locations:?}"
        );
    }
    let method = server
        .hover_type(
            "src/format.js",
            Position {
                line: 1,
                character: 39,
            },
        )
        .unwrap()
        .unwrap();
    assert!(
        method.contains("toFixed") && method.contains("string"),
        "{method}"
    );
    pool.shutdown().unwrap();
    assert_eq!(
        fs::read_to_string(project.0.join("src/quote.ts")).unwrap(),
        source
    );
    assert_eq!(
        fs::read_to_string(project.0.join("src/money.ts")).unwrap(),
        provider
    );
}
