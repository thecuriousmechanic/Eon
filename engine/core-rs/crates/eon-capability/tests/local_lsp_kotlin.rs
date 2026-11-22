#![cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu", target_pointer_width = "64"))]

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

fn sample(label: &str, mut run: impl FnMut(usize)) -> Duration {
    let mut times = [Duration::ZERO; 20];
    for (i, t) in times.iter_mut().enumerate() {
        let start = Instant::now();
        run(i);
        *t = start.elapsed();
    }
    times.sort_unstable();
    eprintln!("{label} p95/20: {:?}", times[18]);
    times[18]
}

#[test]
#[ignore = "requires pinned fwcd image and delegated capped cgroup v2"]
fn confined_fwcd_snapshots() {
    let image = PathBuf::from(std::env::var_os("EON_TEST_KOTLIN").unwrap());
    assert_eq!(image.file_name().unwrap(), "fwcd-kotlin-language-server");
    let pin = std::env::var("EON_TEST_KOTLIN_SHA256").unwrap();
    assert_eq!(pin.len(), 64);
    let pin = std::array::from_fn(|i| u8::from_str_radix(&pin[i*2..i*2+2],16).unwrap());
    let mine = fs::read_to_string("/proc/self/cgroup").unwrap();
    let group = Path::new("/sys/fs/cgroup").join(mine.trim().strip_prefix("0::/").unwrap());
    let config = |digest| OperatorServer::new(ServerKind::Kotlin, &image, &[], Duration::from_secs(12))
        .unwrap().with_kotlin_task_group(&group, digest).unwrap();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target").canonicalize().unwrap();
    let project = Project(dir.join(format!("eon-fwcd-{:016x}", getrandom::u64().unwrap())));
    fs::DirBuilder::new().mode(0o700).create(&project.0).unwrap();
    let a = "package calc\nfun twice(n: Int) = listOf(n, n).sum()\nfun twice(s: String) = s.repeat(2)\n";
    let b = "package client\r\nimport calc.twice\r\n/*𝄞*/ val result: String = twice(3)\r\nval other = twice(\"ok\")\r\n";
    fs::write(project.0.join("Math.kt"), a).unwrap();
    fs::write(project.0.join("Quote.kt"), b).unwrap();
    fs::write(project.0.join("Unopened.kt"), "package client\nval diskOnly = 1\n").unwrap();
    fs::write(project.0.join("build.gradle.kts"), "error(\"unexpected build\")").unwrap();
    let bad = LocalLspPool::start(&[config([0; 32])], &project.0).err().unwrap();
    assert!(bad.to_string().contains("unapproved Kotlin image digest"), "{bad}");
    let start = Instant::now();
    let mut pool = LocalLspPool::start(&[config(pin)], &project.0).unwrap();
    let server = pool.session(ServerKind::Kotlin).unwrap();
    assert!(server.open("build.gradle.kts").is_err());
    server.open("Math.kt").unwrap();
    server.open("Quote.kt").unwrap();
    let errors = server.diagnostics("Quote.kt").unwrap();
    assert_eq!(errors.len(), 1, "{errors:?}");
    let at = Position { line: 2, character: 28 };
    assert_eq!(errors[0].range.start, at);
    assert!(errors[0].message.contains("String") && errors[0].message.contains("Int"), "{errors:?}");
    eprintln!("fwcd launch to first semantic diagnostic: {:?}", start.elapsed());
    assert!(server.diagnostics("Math.kt").unwrap().is_empty());
    let def = sample("fwcd definition", |_| {
        let loc = server.goto_definition("Quote.kt", at).unwrap();
        assert_eq!(loc.len(), 1, "{loc:?}");
        assert_eq!(loc[0].relative_path, "Math.kt");
        assert_eq!(loc[0].range.start, Position { line: 1, character: 4 });
    });
    let refs = sample("fwcd references", |_| {
        let uses = server.find_references("Quote.kt", at).unwrap();
        assert!(uses.iter().any(|l| l.relative_path == "Quote.kt" && l.range.start == at), "{uses:?}");
        assert!(uses.iter().any(|l| l.relative_path == "Math.kt" && l.range.start.line == 1), "{uses:?}");
        assert!(!uses.iter().any(|l| l.range.start.line == 3 || (l.relative_path == "Math.kt" && l.range.start.line == 2)));
    });
    let hover = sample("fwcd hover", |_| {
        let h = server.hover_type("Quote.kt", at).unwrap().unwrap();
        assert!(h.contains("twice(n: Int): Int"), "{h}");
    });
    let diag = sample("fwcd warm diagnostics", |_| assert_eq!(server.diagnostics("Quote.kt").unwrap(), errors));
    assert!(def < Duration::from_millis(30) && [refs, hover, diag].iter().all(|d| *d < Duration::from_millis(50)));
    let fixed = b.replace("result: String", "result: Int");
    sample("fwcd edit to fresh diagnostic", |i| {
        server.change("Quote.kt", if i % 2 == 0 { &fixed } else { b }).unwrap();
        let actual = server.diagnostics("Quote.kt").unwrap();
        if i % 2 == 0 { assert!(actual.is_empty(), "{actual:?}"); } else { assert_eq!(actual, errors); }
    });
    server.change("Math.kt", &a.replace("listOf(n, n).sum()", "n.toString()")).unwrap();
    assert!(server.diagnostics("Quote.kt").unwrap().is_empty());
    assert!(server.hover_type("Quote.kt", at).unwrap().unwrap().contains("twice(n: Int): String"));
    server.close("Math.kt").unwrap();
    assert!(!server.diagnostics("Quote.kt").unwrap().is_empty());
    server.open("Math.kt").unwrap();
    assert_eq!(server.diagnostics("Quote.kt").unwrap(), errors);
    server.change("Quote.kt","package client\nval x = diskOnly").unwrap();
    assert!(server.diagnostics("Quote.kt").unwrap().iter().any(|d| d.message.contains("diskOnly")));
    // fwcd's default URI parser form-decodes '%' and '+'. A returned location
    // must instead identify the exact caller-opened wire URI, even after edits.
    let odd = "A +%π.kt";
    fs::write(project.0.join(odd), "package calc\nfun odds() = listOf(1,3)\n").unwrap();
    server.open(odd).unwrap();
    server.change("Quote.kt","import calc.odds\nval n = odds().sum()").unwrap();
    assert!(server.diagnostics("Quote.kt").unwrap().is_empty());
    let loc = server.goto_definition("Quote.kt", Position {line:1, character:9}).unwrap();
    assert_eq!(loc.len(), 1);
    assert_eq!(loc[0].relative_path, odd);
    let uses = server.find_references(odd, Position {line:1, character:5}).unwrap();
    assert!(uses.iter().any(|l| l.relative_path == odd && l.range.start.line == 1));
    assert!(uses.iter().any(|l| l.relative_path == "Quote.kt" && l.range.start.line == 1));
    server.close(odd).unwrap();
    server.change("Quote.kt", &(0..270).map(|i| format!("val n{i} = diskOnly\n")).collect::<String>()).unwrap();
    assert!(server.diagnostics("Quote.kt").is_err());
    server.change("Quote.kt", b).unwrap();
    assert_eq!(server.diagnostics("Quote.kt").unwrap(), errors);
    // This is a valid filesystem path (< 2 KiB, components < NAME_MAX),
    // but every '%' needs three bytes on the wire. Reject it locally.
    let deep = (0..7).map(|_| "%".repeat(200)).collect::<Vec<_>>().join("/");
    fs::create_dir_all(project.0.join(&deep)).unwrap();
    let overlong = format!("{deep}/Long.kt");
    fs::write(project.0.join(&overlong), "val shouldNotEnterSession = 1\n").unwrap();
    let refused = server.open(&overlong).unwrap_err();
    assert_eq!(refused.kind(), std::io::ErrorKind::InvalidData);
    assert!(refused.to_string().contains("encoded fwcd document URI"), "{refused}");
    assert_eq!(server.diagnostics("Quote.kt").unwrap(), errors);

    // Alternate inferred return types in unsaved edits. Each caller report
    // must use the current callee, not a stale module or the on-disk source.
    let edit_start = Instant::now();
    for i in 0..256 {
        let body = if i % 2 == 0 {
            format!("listOf(n, {i}).sum()")
        } else {
            format!("\"edit-{i}\" + n")
        };
        server.change("Math.kt", &a.replace("listOf(n, n).sum()", &body)).unwrap();
        let actual = server.diagnostics("Quote.kt").unwrap();
        if i % 2 == 0 {
            assert_eq!(actual, errors, "edit {i}");
        } else {
            assert!(actual.is_empty(), "edit {i}: {actual:?}");
        }
    }
    eprintln!("256 distinct callee edits + fresh caller reports: {:?}", edit_start.elapsed());
    assert!(server.hover_type("Quote.kt", at).unwrap().unwrap().contains("twice(n: Int): String"));
    pool.shutdown().unwrap();
    assert_eq!(fs::read_to_string(project.0.join("Quote.kt")).unwrap(), b);
}

#[test]
#[ignore = "requires pinned Kotlin image and delegated capped cgroup v2"]
fn confined_kotlin_snapshots() {
    let image = PathBuf::from(std::env::var_os("EON_TEST_KOTLIN").unwrap());
    let pin = std::env::var("EON_TEST_KOTLIN_SHA256").unwrap();
    assert_eq!(pin.len(), 64);
    let pin = std::array::from_fn(|i| u8::from_str_radix(&pin[i * 2..i * 2 + 2], 16).unwrap());
    let mine = fs::read_to_string("/proc/self/cgroup").unwrap();
    let group = Path::new("/sys/fs/cgroup").join(mine.trim().strip_prefix("0::/").unwrap());
    let config = |digest| OperatorServer::new(ServerKind::Kotlin, &image, &[], Duration::from_secs(12))
        .unwrap().with_kotlin_task_group(&group, digest).unwrap();

    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target").canonicalize().unwrap();
    let project = Project(dir.join(format!("eon-kotlin-{:016x}", getrandom::u64().unwrap())));
    fs::DirBuilder::new().mode(0o700).create(&project.0).unwrap();
    let a = "package accounting\nfun twice(n: Int): Int = n * 2\nfun twice(s: String): String = s\n";
    let b = "package client\r\nimport accounting.twice\r\n/*𝄞*/ val result: String = twice(3)\r\nval other = twice(\"OK\")\r\n";
    let fixed = b.replace("result: String", "result: Int");
    fs::write(project.0.join("Math.kt"), a).unwrap();
    fs::write(project.0.join("Quote.kt"), b).unwrap();
    fs::write(project.0.join("Unopened.kt"), "package client\nval onlyOnDisk = 1\n").unwrap();
    fs::write(project.0.join("build.gradle.kts"), "error(\"build script was evaluated\")").unwrap();

    let denied = LocalLspPool::start(&[config([0; 32])], &project.0).err().unwrap();
    assert!(denied.to_string().contains("unapproved Kotlin image digest"), "{denied}");
    let start = Instant::now();
    let mut pool = LocalLspPool::start(&[config(pin)], &project.0).unwrap();
    let server = pool.session(ServerKind::Kotlin).unwrap();
    assert!(server.open("build.gradle.kts").is_err());
    server.open("Math.kt").unwrap();
    server.open("Quote.kt").unwrap();
    let errors = server.diagnostics("Quote.kt").unwrap();
    let first = Position { line: 2, character: 28 };
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].range.start, first);
    assert!(errors[0].message.contains("Int") && errors[0].message.contains("String"));
    eprintln!("launch to first semantic diagnostic: {:?}", start.elapsed());
    assert!(server.diagnostics("Math.kt").unwrap().is_empty());

    let definitions = sample("definition", |_| {
        let loc = server.goto_definition("Quote.kt", first).unwrap();
        assert_eq!(loc.len(), 1, "{loc:?}");
        assert_eq!(loc[0].relative_path, "Math.kt");
        assert_eq!(loc[0].range.start, Position { line: 1, character: 4 });
    });
    assert!(definitions < Duration::from_millis(30), "definition p95: {definitions:?}");
    sample("references", |_| {
        let loc = server.find_references("Quote.kt", first).unwrap();
        assert!(loc.iter().any(|l| l.relative_path == "Quote.kt" && l.range.start == first), "{loc:?}");
        assert!(loc.iter().any(|l| l.relative_path == "Math.kt" && l.range.start.line == 1), "{loc:?}");
        assert!(!loc.iter().any(|l| l.range.start.line == 3 || (l.relative_path == "Math.kt" && l.range.start.line == 2)));
    });
    sample("hover", |_| {
        let hover = server.hover_type("Quote.kt", first).unwrap().unwrap();
        assert!(hover.contains("twice(n: Int): Int"), "{hover}");
    });
    let diagnostics = sample("warm diagnostic", |_| assert_eq!(server.diagnostics("Quote.kt").unwrap(), errors));
    assert!(diagnostics < Duration::from_millis(50), "warm diagnostics p95: {diagnostics:?}");
    sample("edit to fresh diagnostic", |n| {
        server.change("Quote.kt", if n % 2 == 0 { &fixed } else { b }).unwrap();
        let current = server.diagnostics("Quote.kt").unwrap();
        if n % 2 == 0 { assert!(current.is_empty(), "{current:?}"); } else { assert_eq!(current, errors); }
    });
    // An edit to *another* buffer invalidates this consumer's report and hover.
    server.change("Math.kt", &a.replace("Int = n * 2", "String = \"ok\"")).unwrap();
    assert!(server.diagnostics("Quote.kt").unwrap().is_empty());
    assert!(server.hover_type("Quote.kt", first).unwrap().unwrap().contains("twice(n: Int): String"));
    let symbols = server.workspace_symbols("TwC").unwrap();
    assert_eq!(symbols.len(), 2, "{symbols:?}");
    for sym in &symbols {
        assert_eq!(sym.name, "twice");
        assert_eq!(sym.kind, 12);
        assert_eq!(sym.location.relative_path, "Math.kt");
        assert!(matches!(sym.location.range.start.line, 1 | 2));
    }
    assert!(server.workspace_symbols("olyOnD").unwrap().is_empty());
    assert!(server.workspace_symbols("").is_err());
    server.close("Math.kt").unwrap();
    assert!(server.workspace_symbols("TwC").unwrap().is_empty());
    assert!(server.goto_definition("Quote.kt", first).unwrap().is_empty());
    assert!(server.diagnostics("Quote.kt").unwrap().iter().any(|e| e.message.contains("Unresolved reference")));
    server.open("Math.kt").unwrap();
    assert_eq!(server.diagnostics("Quote.kt").unwrap(), errors);
    server.change("Quote.kt", "package client\nval x = onlyOnDisk").unwrap();
    let unopened = server.diagnostics("Quote.kt").unwrap();
    assert!(unopened.iter().any(|e| e.message.contains("onlyOnDisk")), "{unopened:?}");
    server.change("Quote.kt", "package client\nval x = ").unwrap();
    assert!(!server.diagnostics("Quote.kt").unwrap().is_empty());
    let many = (0..300).map(|i| format!("val property{i} = 1\n")).collect::<String>();
    server.change("Quote.kt", &many).unwrap();
    assert_eq!(server.workspace_symbols("prpty").unwrap().len(), 256);
    // Built-ins alone do not contain stdlib functions like listOf. Don't
    // present an unresolved stdlib import as a diagnostic from the user's build.
    server.change("Quote.kt", "val values = listOf(1)").unwrap();
    let stdlib = server.diagnostics("Quote.kt").unwrap();
    assert!(stdlib.iter().any(|e| e.message.contains("built-ins only") && e.message.contains("listOf")));
    let operator = "class Box { operator fun get(i: Int): Int = i }\nval n = Box()[2]";
    server.change("Quote.kt", operator).unwrap();
    assert!(server.diagnostics("Quote.kt").unwrap().is_empty());
    for (line, needle) in [(0, "get"), (1, "[")] {
        let character = operator.lines().nth(line).unwrap().find(needle).unwrap();
        assert!(server.find_references("Quote.kt", Position { line: line as _, character: character as _ }).is_err());
    }
    // A report or reference list beyond the client's bound must not be
    // truncated or poison the session. A byte overflow below 256 items too.
    let uses = (0..270).map(|i| format!("val item{i} = twice(3)\n")).collect::<String>();
    server.change("Quote.kt", &format!("import accounting.twice\n{uses}")).unwrap();
    assert!(server.find_references("Math.kt", Position { line: 1, character: 4 }).is_err());
    server.change("Quote.kt", &uses).unwrap();
    assert!(server.diagnostics("Quote.kt").is_err());
    let names = "q".repeat(950);
    let big = (0..55).map(|i| format!("val {names}{i} = 1\n")).collect::<String>();
    server.change("Math.kt", &big).unwrap();
    server.change("Quote.kt", &big).unwrap();
    assert!(server.workspace_symbols("qq").is_err());
    server.change("Math.kt", a).unwrap();
    server.change("Quote.kt", b).unwrap();
    assert_eq!(server.diagnostics("Quote.kt").unwrap(), errors);
    pool.shutdown().unwrap();
    assert_eq!(fs::read_to_string(project.0.join("Quote.kt")).unwrap(), b);
    assert_eq!(fs::read_to_string(project.0.join("Math.kt")).unwrap(), a);
}
