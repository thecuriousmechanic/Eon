use super::*;
use std::fs;
use std::os::unix::fs::symlink;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
        fs::create_dir_all(&parent).unwrap();
        let root = parent
            .canonicalize()
            .unwrap()
            .join(format!("eon-lsp-test-{:016x}", getrandom::u64().unwrap()));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a.py"), "def f():\n    return '🙂'\n").unwrap();
        fs::write(root.join("b.py"), "def g():\n    return 2\n").unwrap();
        Self(root)
    }

    fn start(&self, mode: &str) -> LocalLsp {
        self.try_start(mode, Duration::from_secs(3)).unwrap()
    }

    fn try_start(&self, mode: &str, deadline: Duration) -> io::Result<LocalLsp> {
        // Fixed test fixture, never part of production config. -I -S prevents
        // imports from the workspace, user site, environment or startup hooks.
        let config = OperatorServer {
            kind: ServerKind::Pyright,
            executable: Path::new("/usr/bin/python3").canonicalize().unwrap(),
            runtime: ["/usr/lib", "/lib64"]
                .iter()
                .map(|s| Path::new(s).canonicalize().unwrap())
                .collect(),
            deadline,
            rust_task_group: None,
            pyright_group: None,
            vtsls_image: None,
            kotlin_image: None,
        };
        LocalLsp::spawn(
            &config,
            &self.0,
            &[
                "-I",
                "-S",
                "-c",
                include_str!("peer_fixture.py"),
                mode,
                self.0.join("a.py").to_str().unwrap(),
            ],
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

const POS: Position = Position { line: 0, character: 0 };

#[test]
fn failed_or_stalled_handshake_never_returns_a_ready_session() {
    let f = Fixture::new();
    for (mode, expected) in [
        ("bad-capabilities", io::ErrorKind::InvalidData),
        ("exit-before-ready", io::ErrorKind::UnexpectedEof),
        ("stall-before-ready", io::ErrorKind::TimedOut),
    ] {
        let deadline = if mode == "stall-before-ready" {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(3)
        };
        let error = f.try_start(mode, deadline).err().expect("handshake should fail");
        assert_eq!(error.kind(), expected, "{mode}: {error}");
    }
}

#[test]
fn operator_configuration_is_explicit_and_never_uses_a_shell_or_script() {
    let f = Fixture::new();
    let timeout = Duration::from_secs(1);
    for path in ["rust-analyzer", "/bin/sh"] {
        assert!(OperatorServer::new(ServerKind::RustAnalyzer, Path::new(path), &[], timeout).is_err());
    }
    let script = f.0.join("rust-analyzer");
    fs::write(&script, "#!/bin/sh\n").unwrap();
    assert!(OperatorServer::new(ServerKind::RustAnalyzer, &script, &[], timeout).is_err());
    for (kind, name) in [(ServerKind::Kotlin, "kotlin-language-server"), (ServerKind::Vtsls, "vtsls")] {
        let launcher = f.0.join(name);
        fs::write(&launcher, "#!/bin/sh\n").unwrap();
        assert!(OperatorServer::new(kind, &launcher, &[], timeout).is_err());
    }
    let image = f.0.join("vtsls");
    let kotlin = f.0.join("kotlin-language-server");
    fs::copy("/usr/bin/true", &kotlin).unwrap();
    assert!(OperatorServer::new(ServerKind::Kotlin, &kotlin, &[PathBuf::from("/usr")], timeout).is_err());
    let unpinned = OperatorServer::new(ServerKind::Kotlin, &kotlin, &[], timeout).unwrap();
    let err = LocalLsp::start(&unpinned, &f.0).err().unwrap();
    assert!(err.to_string().contains("pinned image"));
    assert!(unpinned.with_kotlin_task_group(Path::new("relative"), [0; 32]).is_err());
    fs::copy("/usr/bin/true", &image).unwrap();
    let unpinned = OperatorServer::new(ServerKind::Vtsls, &image, &[], timeout).unwrap();
    let err = LocalLsp::start(&unpinned, &f.0).err().unwrap();
    assert!(err.to_string().contains("pinned image"));
    assert!(unpinned.with_vtsls_task_group(Path::new("relative"), [0; 32]).is_err());
    let mut empty = LocalLspPool::start(&[], &f.0).unwrap();
    assert!(empty.session(ServerKind::RustAnalyzer).is_none());
    empty.shutdown().unwrap();
    assert_eq!(
        virtual_uri("/virtual", "pkg/ℹ 50%#.py"),
        "file:///virtual/pkg/%E2%84%B9%2050%25%23.py"
    );
}

fn reaped(pid: u32) {
    let mut status = 0;
    // SAFETY: this is only observing our test child and providing writable status storage.
    assert_eq!(unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) }, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ECHILD));
}

#[test]
fn scoped_stdio_session_answers_read_only_queries_and_shuts_down() {
    let f = Fixture::new();
    let mut client = f.start("pull");
    let pid = client.child.0.as_ref().unwrap().id();
    client.open("a.py").unwrap();
    client.open("b.py").unwrap();
    assert!(client.open("a.py").is_err());
    assert!(client.open("../a.py").is_err());
    assert!(client.open(f.0.join("a.py").to_str().unwrap()).is_err());
    assert!(client.hover_type("unknown.py", POS).is_err());
    assert_eq!(client.hover_type("a.py", POS).unwrap().as_deref(), Some("int"));
    assert_eq!(client.goto_definition("a.py", POS).unwrap()[0].relative_path, "b.py");
    assert_eq!(client.find_references("a.py", POS).unwrap().len(), 2);
    assert_eq!(client.diagnostics("a.py").unwrap()[0].message, "fixture message");
    client.shutdown().unwrap();
    reaped(pid);
    assert_eq!(
        fs::read_to_string(f.0.join("a.py")).unwrap(),
        "def f():\n    return '🙂'\n"
    );
}

#[test]
fn pull_session_ignores_unsolicited_pushes_but_bounds_requested_reports() {
    let f = Fixture::new();
    let mut client = f.start("pull-with-pushes");
    client.open("a.py").unwrap();
    assert_eq!(client.hover_type("a.py", POS).unwrap().as_deref(), Some("int"));
    assert!(client.docs[&client.uri("a.py").unwrap()].diagnostics.is_none());
    assert_eq!(client.diagnostics("a.py").unwrap_err().kind(), io::ErrorKind::InvalidData);
    client.shutdown().unwrap();
}

#[test]
fn opaque_documents_reject_external_locations_and_cache_push_diagnostics() {
    let f = Fixture::new();
    let mut push = f.start("push");
    push.open("a.py").unwrap();
    let first = push.diagnostics("a.py").unwrap();
    assert_eq!(first, push.diagnostics("a.py").unwrap());
    assert_eq!(first[0].range.start, POS);
    push.shutdown().unwrap();
    let mut outside = f.start("outside");
    outside.open("a.py").unwrap();
    assert_eq!(
        outside.goto_definition("a.py", POS).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(outside.workspace_symbols("f").unwrap_err().kind(), io::ErrorKind::InvalidData);
    let pid = outside.child.0.as_ref().unwrap().id();
    drop(outside);
    reaped(pid);
}

#[test]
fn full_and_incremental_changes_invalidate_pushes_and_close_discards_state() {
    let f = Fixture::new();
    let before = "a\r\nb\r🙂x";
    fs::write(f.0.join("a.py"), before).unwrap();
    for mode in ["full", "incremental"] {
        let mut client = f.start(mode);
        client.open("a.py").unwrap();
        assert_eq!(client.diagnostics("a.py").unwrap().len(), 1);
        assert!(client.change("a.py", &"x".repeat(MAX_DOCUMENT + 1)).is_err());
        client.change("a.py", "updated").unwrap();
        assert!(client.diagnostics("a.py").unwrap().is_empty());
        client.close("a.py").unwrap();
        assert!(client.diagnostics("a.py").is_err());
        assert!(client.change("a.py", "").is_err());
        assert!(client.close("a.py").is_err());
        client.open("a.py").unwrap();
        assert_eq!(client.diagnostics("a.py").unwrap().len(), 1);
        client.shutdown().unwrap();
    }
    assert_eq!(fs::read_to_string(f.0.join("a.py")).unwrap(), before);
}

#[test]
fn an_old_or_unversioned_push_cannot_satisfy_an_edited_or_reopened_snapshot() {
    let f = Fixture::new();
    fs::write(f.0.join("a.py"), "a\r\nb\r🙂x").unwrap();
    for reopen in [false, true] {
        let mut client = f.start("stale");
        client.open("a.py").unwrap();
        assert_eq!(client.diagnostics("a.py").unwrap().len(), 1);
        if reopen {
            client.close("a.py").unwrap();
            client.open("a.py").unwrap();
        } else {
            client.change("a.py", "updated").unwrap();
        }
        client.deadline = Duration::from_millis(100);
        assert_eq!(client.diagnostics("a.py").unwrap_err().kind(), io::ErrorKind::TimedOut);
    }
}

#[test]
fn capabilities_gate_requests_and_admit_scoped_definition_links_and_marked_strings() {
    let f = Fixture::new();
    let mut client = f.start("no-sync");
    assert_eq!(client.open("a.py").unwrap_err().kind(), io::ErrorKind::Unsupported);
    client.shutdown().unwrap();
    let mut client = f.start("immutable");
    client.open("a.py").unwrap();
    for err in [
        client.change("a.py", "").unwrap_err(),
        client.hover_type("a.py", POS).unwrap_err(),
        client.goto_definition("a.py", POS).unwrap_err(),
        client.find_references("a.py", POS).unwrap_err(),
        client.workspace_symbols("f").unwrap_err(),
    ] {
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }
    client.shutdown().unwrap();
    let mut client = f.start("link");
    client.open("a.py").unwrap();
    assert_eq!(client.goto_definition("a.py", POS).unwrap()[0].relative_path, "a.py");
    assert_eq!(client.workspace_symbols("f").unwrap()[0].location.relative_path, "a.py");
    assert_eq!(client.hover_type("a.py", POS).unwrap().as_deref(), Some("first\nint"));
    client.shutdown().unwrap();
}

#[test]
fn corrupt_responses_and_unrelated_traffic_poison_the_session() {
    let f = Fixture::new();
    for mode in ["flood", "mismatch"] {
        let mut client = f.start(mode);
        let pid = client.child.0.as_ref().unwrap().id();
        client.open("a.py").unwrap();
        assert_eq!(
            client.hover_type("a.py", POS).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            client.hover_type("a.py", POS).unwrap_err().kind(),
            io::ErrorKind::NotConnected
        );
        reaped(pid);
    }
}

#[test]
fn incomplete_body_blocked_input_and_ignored_exit_are_bounded_and_reaped() {
    let f = Fixture::new();
    for mode in ["timeout", "blocked-stdin", "ignore-exit"] {
        let mut client = f.start(mode);
        client.deadline = Duration::from_millis(100);
        let pid = client.child.0.as_ref().unwrap().id();
        let start = Instant::now();
        let err = match mode {
            "timeout" => {
                client.open("a.py").unwrap();
                client.hover_type("a.py", POS).unwrap_err()
            }
            "blocked-stdin" => {
                // Escaping control characters makes this larger than a default
                // pipe even on kernels with a 64 KiB initial pipe capacity.
                fs::write(f.0.join("many.py"), vec![1; MAX_DOCUMENT]).unwrap();
                client.open("many.py").unwrap_err()
            }
            _ => client.shutdown().unwrap_err(),
        };
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "{mode} exceeded the outer test budget"
        );
        reaped(pid);
    }
}

#[test]
fn failed_did_open_retires_markers_and_dead_session_cannot_repopulate_them() {
    let f = Fixture::new();
    let state = Fixture::new();
    let mut client = f.start("fail-during-open");
    client.wire.limit_test_pipe();
    let catalog = pyright::ModuleNames::new(&state.0, &f.0).unwrap();
    let root = catalog.root.clone();
    client.virtual_root = root.to_str().unwrap().into();
    client.python_names = Some(catalog);
    let pid = client.child.0.as_ref().unwrap().id();
    client.open("a.py").unwrap();
    assert_eq!(fs::metadata(root.join("a.py")).unwrap().len(), 0);

    // Too large for the bounded pipe after JSON escaping: the peer closes
    // stdin after reading the header and the start of this didOpen payload.
    fs::write(f.0.join("many.py"), vec![1; MAX_DOCUMENT]).unwrap();
    assert_eq!(client.open("many.py").unwrap_err().kind(), io::ErrorKind::BrokenPipe);
    let receipt = client.wire.recv(client.end()).unwrap();
    assert_eq!(receipt["method"], "test/partialDidOpen");
    assert_eq!(receipt["params"]["uri"], virtual_uri(&client.virtual_root, "many.py"));
    reaped(pid);
    assert!(!root.try_exists().unwrap());
    assert!(client.python_names.is_none());
    assert_eq!(client.docs.len(), 1); // no document was published for the failed open
    let version = client.version;
    for i in 0..=MAX_OPEN {
        let name = format!("retry{i}.py");
        fs::write(f.0.join(&name), "").unwrap();
        assert_eq!(client.open(&name).unwrap_err().kind(), io::ErrorKind::NotConnected);
    }
    // Also reject before trying to snapshot even a missing document.
    assert_eq!(client.open("missing.py").unwrap_err().kind(), io::ErrorKind::NotConnected);
    assert_eq!(client.version, version);
    assert!(!root.try_exists().unwrap());
}

#[test]
fn module_catalog_caps_markers_independently_of_document_admission() {
    let f = Fixture::new();
    let state = Fixture::new();
    let mut catalog = pyright::ModuleNames::new(&state.0, &f.0).unwrap();
    for i in 0..MAX_OPEN {
        catalog.insert(&format!("module{i}.py")).unwrap();
    }
    assert!(catalog.insert("overflow.py").is_err());
    assert!(!catalog.root.join("overflow.py").try_exists().unwrap());
    assert_eq!(fs::read_dir(&catalog.root).unwrap().count(), MAX_OPEN);
    catalog.remove("module0.py").unwrap();
    catalog.insert("replacement.py").unwrap();
    assert_eq!(fs::read_dir(&catalog.root).unwrap().count(), MAX_OPEN);
}

#[test]
fn marker_cleanup_failure_after_did_close_retires_the_session() {
    let f = Fixture::new();
    let state = Fixture::new();
    let mut client = f.start("blocked-stdin");
    let catalog = pyright::ModuleNames::new(&state.0, &f.0).unwrap();
    let root = catalog.root.clone();
    client.python_names = Some(catalog);
    client.open("a.py").unwrap();
    fs::remove_file(root.join("a.py")).unwrap();
    assert_eq!(client.close("a.py").unwrap_err().kind(), io::ErrorKind::NotFound);
    assert_eq!(client.open("b.py").unwrap_err().kind(), io::ErrorKind::NotConnected);
    assert!(!root.try_exists().unwrap());
}

#[test]
fn preview_changes_only_unique_top_level_block_and_never_writes() {
    let f = Fixture::new();
    let w = Workspace::open(&f.0).unwrap();
    let before = "// keep\nfn first() {}\nfn f() { /* old */ }\n";
    fs::write(f.0.join("a.rs"), before).unwrap();
    let p = preview_replacement(&w, "a.rs", "f", "fn f() { /* new */ }").unwrap();
    assert_eq!(p.before, before);
    assert_eq!(p.after, "// keep\nfn first() {}\nfn f() { /* new */ }\n");
    assert_eq!(p.expected_sha256, w.read_range("a.rs", 0, MAX_DOCUMENT).unwrap().sha256);
    assert_eq!(fs::read_to_string(f.0.join("a.rs")).unwrap(), before);
    let p = preview_replacement(&w, "a.py", "f", "def f():\n    return 1").unwrap();
    assert_eq!(p.after, "def f():\n    return 1\n");
    for bad in [
        "fn nope() {}",
        "fn f() {",
        "fn f() {}\nfn sneaked() {}",
        "const A:u8=3;\nfn f() {}",
    ] {
        assert!(preview_replacement(&w, "a.rs", "f", bad).is_err());
    }
    for source in ["fn f() {}\nfn f() {}", "mod m { fn f() {} }", "fn f(){}\nfn broken("] {
        fs::write(f.0.join("a.rs"), source).unwrap();
        assert!(preview_replacement(&w, "a.rs", "f", "fn f() { 1; }").is_err());
    }
}

#[test]
fn python_preview_rejects_conflicting_declarations_but_preserves_unrelated_ones() {
    let f = Fixture::new();
    let w = Workspace::open(&f.0).unwrap();
    let path = f.0.join("a.py");
    let ordinary = "async def f(): return 1\n";
    let replacement = "async def f(): return 3";
    for conflict in ["@wrap\ndef f(): return 2\n", "class f: pass\n", "@wrap\nclass f: pass\n"] {
        for source in [format!("{ordinary}{conflict}"), format!("{conflict}{ordinary}")] {
            fs::write(&path, &source).unwrap();
            assert!(preview_replacement(&w, "a.py", "f", replacement).is_err(), "{source}");
        }
    }

    let prefix = "@wrap\nclass C:\n    def f(self): pass\n\n";
    let suffix = "\n@wrap\nasync def g(): pass\n";
    let source = format!("{prefix}{ordinary}{suffix}");
    fs::write(&path, &source).unwrap();
    let preview = preview_replacement(&w, "a.py", "f", replacement).unwrap();
    assert_eq!(preview.after, format!("{prefix}{replacement}\n{suffix}"));
    assert_eq!(fs::read_to_string(path).unwrap(), source);
}

#[test]
fn ecma_preview_requires_one_unambiguous_concrete_top_level_declaration() {
    let f = Fixture::new();
    let w = Workspace::open(&f.0).unwrap();
    let replacement = "function f() { return 1; }";
    for (path, original) in [
        (
            "a.ts",
            "export function f(x: string): number;\nexport function f(x: string) { return 0; }",
        ),
        (
            "a.ts",
            "export declare /* signature */ function f(): void;\nexport function f() {}",
        ),
        ("a.ts", "declare namespace Helpers { function f(): void; }"),
        ("a.js", "export default function() {}"),
        ("a.js", "export default (function f() {});"),
        ("a.js", "const f = () => 0;"),
        ("a.js", "class C { f() {} }"),
        ("a.js", "function outer() { function f() {} }"),
        ("a.js", "const sample = `function f() {}`; // function f() {}"),
        ("a.js", "function f() {} function f() {}"),
        ("a.js", r"function \u0066() {} function f() {}"),
        ("a.js", "function f() {} function unfinished("),
        ("a.js", "function f(x: number) {}"),
        ("a.ts", "function f() { return <article/>; }"),
    ] {
        fs::write(f.0.join(path), original).unwrap();
        assert!(
            preview_replacement(&w, path, "f", replacement).is_err(),
            "{path}: {original}"
        );
    }
    for (path, prefix) in [("a.mjs", "export "), ("a.cjs", ""), ("a.mts", "export "), ("a.cts", "")] {
        let original = format!(
            "// function f(): 🗓\r\nfunction outer() {{ function f() {{}} }}\r\n\
             {prefix}/* keep */ async function* f() {{ yield 0; }} // keep\r\nconst end = 0;\r\n"
        );
        fs::write(f.0.join(path), &original).unwrap();
        let p = preview_replacement(&w, path, "f", "async function* f() { yield 2; }\n").unwrap();
        assert_eq!(p.after, original.replace("yield 0", "yield 2"));
        for bad in [
            "async function* f() { yield 0; }",
            "function renamed() {}",
            "export default function f() {}",
            "/* outside */ function f() {}",
            "function f() {} // outside",
            "function f() {} function extra() {}",
            "function f() {",
        ] {
            assert!(preview_replacement(&w, path, "f", bad).is_err(), "{bad}");
        }
    }
}

#[test]
fn path_scope_and_document_budget_also_apply_to_previews() {
    let f = Fixture::new();
    let other = Fixture::new();
    let w = Workspace::open(&f.0).unwrap();
    symlink(other.0.join("a.py"), f.0.join("link.py")).unwrap();
    let rep = "def f():\n    return 8";
    for path in ["link.py", "../a.py", other.0.join("a.py").to_str().unwrap()] {
        assert!(preview_replacement(&w, path, "f", rep).is_err());
    }
    fs::write(f.0.join("big.py"), vec![b'x'; MAX_DOCUMENT + 1]).unwrap();
    assert!(snapshot(&w, "big.py").is_err());
    assert!(preview_replacement(&w, "a.py", "f", &"x".repeat(MAX_DOCUMENT + 1)).is_err());
    assert_eq!(
        fs::read_to_string(other.0.join("a.py")).unwrap(),
        "def f():\n    return '🙂'\n"
    );
    let mut config = OperatorServer {
        kind: ServerKind::Pyright,
        executable: f.0.join("a.py"),
        runtime: vec![],
        deadline: Duration::from_secs(1),
        rust_task_group: None,
        pyright_group: None,
        vtsls_image: None,
        kotlin_image: None,
    };
    assert!(LocalLsp::start(&config, &f.0).is_err());
    config.executable = PathBuf::from("/operator-only/server");
    config.runtime.push(other.0.parent().unwrap().to_path_buf());
    assert!(LocalLsp::start(&config, &f.0).is_err());
}
