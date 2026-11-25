//! Read-only, synchronous stdio LSP sessions for explicitly opened source snapshots.
//! Server provisioning is a trusted host operation, not a tool parameter. Installations and
//! runtime paths must be operator-owned and immutable to project authors. No workspace
//! root on disk, real workspace URI, host environment, config discovery, shell, or command RPC is provided.
//! Pyright can enumerate a private directory of empty module-name markers;
//! it receives file contents exclusively through the stdio overlay.
//! The pinned vtsls image runs its TypeScript server in-process. Its inferred
//! project contains only open snapshots and the embedded TypeScript library.
//! Kotlin supports a built-ins-only worker or the pinned fwcd snapshot image
//! with fixed stdlib/JVM data. Neither discovers or builds projects.
//! Servers requiring a disk-backed project or unsupported capabilities may decline queries.
//! The native policy is for trusted workers, not arbitrary binaries. Deadlines bound
//! protocol I/O, not kernel/filesystem latency or the worker's CPU and memory use.
//! Helpers must remain in the worker's process group until it shuts down.
//! The rust-analyzer profile analyzes a dependency-free 2021 library rooted at
//! virtual `src/lib.rs`, using only opened `.rs` snapshots. It does not load Cargo
//! metadata, sysroot, build scripts, proc macros or project configuration.
mod capabilities;
mod preview;
mod pyright;
mod wire;

use std::collections::BTreeMap;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::sandbox::{ConfinementPolicy, WorkerIo, confine_command};
use crate::workspace::Workspace;
use capabilities::{Capabilities, Diagnostics, Sync, require};
pub use preview::{ReplacementPreview, preview_replacement};
use wire::Wire;

const MAX_DOCUMENT: usize = 64 * 1024;
const MAX_OPEN: usize = 16;
const MAX_MESSAGES: usize = 32;
const MAX_RESULTS: usize = 256;

fn invalid(s: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, s)
}

fn timeout() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "local LSP deadline expired")
}

fn snapshot(workspace: &Workspace, path: &str) -> io::Result<(String, [u8; 32])> {
    let read = workspace.read_range(path, 0, MAX_DOCUMENT).map_err(io::Error::other)?;
    if read.file_len != read.bytes.len() as u64 {
        return Err(invalid("document exceeds byte limit or changed during read"));
    }
    let text = String::from_utf8(read.bytes).map_err(|_| invalid("document must be UTF-8"))?;
    Ok((text, read.sha256))
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ServerKind {
    RustAnalyzer,
    Clangd,
    Kotlin,
    Pyright,
    Vtsls,
}

impl ServerKind {
    fn executable_name(self) -> &'static str {
        match self {
            Self::RustAnalyzer => "rust-analyzer",
            Self::Clangd => "clangd",
            Self::Kotlin => "kotlin-language-server",
            Self::Pyright => "pyright-langserver",
            Self::Vtsls => "vtsls",
        }
    }

    fn language(self, path: &str) -> io::Result<&'static str> {
        match (self, Path::new(path).extension().and_then(|s| s.to_str())) {
            (Self::RustAnalyzer, Some("rs")) => Ok("rust"),
            (Self::Clangd, Some("c" | "h")) => Ok("c"),
            (Self::Clangd, Some("cc" | "cpp" | "cxx" | "hpp")) => Ok("cpp"),
            (Self::Kotlin, Some("kt")) => Ok("kotlin"),
            (Self::Pyright, Some("py" | "pyi")) => Ok("python"),
            (Self::Vtsls, Some("ts")) => Ok("typescript"),
            (Self::Vtsls, Some("js")) => Ok("javascript"),
            (Self::Vtsls, Some("tsx")) => Ok("typescriptreact"),
            (Self::Vtsls, Some("jsx")) => Ok("javascriptreact"),
            _ => Err(invalid("document does not match server language")),
        }
    }

    fn args(self) -> &'static [&'static str] {
        match self {
            Self::Pyright | Self::Vtsls => &["--stdio"],
            Self::Clangd => &[
                // Use the built-in fallback, not a disk compilation database
                // (even with --sync, its change broadcaster starts a thread).
                "--compile_args_from=lsp",
                "--sync",
                "--pch-storage=memory",
                "--enable-config=0",
                "--background-index=0",
                "--clang-tidy=0",
            ],
            _ => &[],
        }
    }
}

/// This object is configured once by the operator. Supply a fixed absolute server
/// executable (or vetted native launcher for script-based servers) and only the
/// trusted runtime files/directories it needs. Never put this API in tool dispatch.
pub struct OperatorServer {
    kind: ServerKind,
    executable: PathBuf,
    runtime: Vec<PathBuf>,
    deadline: Duration,
    rust_task_group: Option<PathBuf>,
    pyright_group: Option<pyright::Provision>,
    vtsls_image: Option<(PathBuf, [u8; 32])>,
    kotlin_image: Option<(PathBuf, [u8; 32])>,
}

impl OperatorServer {
    pub fn new(kind: ServerKind, executable: &Path, runtime: &[PathBuf], deadline: Duration) -> io::Result<Self> {
        if kind == ServerKind::RustAnalyzer {
            crate::sandbox::check_rust_analyzer_abi()?;
        }
        if kind == ServerKind::Kotlin && !runtime.is_empty() {
            return Err(invalid("Kotlin does not accept an external runtime"));
        }
        if !executable.is_absolute() || runtime.len() > 24 || deadline.is_zero() || deadline > Duration::from_secs(30) {
            return Err(invalid("invalid server configuration"));
        }
        let executable = executable.canonicalize()?;
        let name = executable.file_name().and_then(|s| s.to_str()).unwrap_or_default();
        let expected = kind.executable_name();
        let fwcd = kind == ServerKind::Kotlin && name == "fwcd-kotlin-language-server";
        if !(fwcd || name == expected
            || name
                .strip_prefix(expected)
                .and_then(|s| s.strip_prefix('-'))
                .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())))
            || !executable.is_file()
        {
            return Err(invalid("expected the configured language server executable"));
        }
        // execvp may interpret unrecognized executables as shell scripts. Only
        // accept native ELF programs, never shebangs or executable project text.
        use std::io::Read;
        let mut magic = [0; 4];
        if kind == ServerKind::Kotlin {
            // Don't block on a FIFO substituted before the final image grant.
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
                .open(&executable)?.read_exact(&mut magic)?;
        } else {
            std::fs::File::open(&executable)?.read_exact(&mut magic)?;
        }
        if &magic != b"\x7fELF" {
            return Err(invalid("server must be a native executable"));
        }
        let runtime = runtime
            .iter()
            .map(|p| {
                if !p.is_absolute() {
                    return Err(invalid("runtime path must be absolute"));
                }
                p.canonicalize()
            })
            .collect::<io::Result<_>>()?;
        Ok(Self {
            kind,
            executable,
            runtime,
            deadline,
            rust_task_group: None,
            pyright_group: None,
            vtsls_image: None,
            kotlin_image: None,
        })
    }

    /// Required for rust-analyzer only. Provision a trusted cgroup v2 with
    /// `pids.max` in 1..=24 and permission to enroll the child. Do not move
    /// workers or raise that limit until reaped. No controller is enabled or
    /// reconfigured here. An unusable group fails launch, without fallback.
    pub fn with_rust_analyzer_task_group(mut self, group: &Path) -> io::Result<Self> {
        if self.kind != ServerKind::RustAnalyzer || !group.is_absolute() {
            return Err(invalid("expected absolute rust-analyzer task group"));
        }
        self.rust_task_group = Some(group.to_owned());
        Ok(self)
    }

    /// Opt in only for the operator-provisioned fixed-entrypoint Pyright SEA
    /// (see `provision/pyright-sea.cjs`), never a Node CLI or script launcher.
    /// `typeshed-fallback/stdlib` must be the corresponding upstream data,
    /// immutable to project authors. Provision the SEA's SHA-256 independently
    /// of launch, a capped cgroup as for rust-analyzer, and an owned state
    /// directory disjoint from the workspace for empty module-name markers.
    pub fn with_pyright_task_group(mut self, group: &Path, state_dir: &Path, image_sha256: [u8; 32]) -> io::Result<Self> {
        crate::sandbox::check_rust_analyzer_abi()?;
        if self.kind != ServerKind::Pyright || !group.is_absolute() || !state_dir.is_absolute() {
            return Err(invalid("expected absolute Pyright task group and state directory"));
        }
        self.pyright_group = Some(pyright::Provision { group: group.to_owned(), state: state_dir.to_owned(), image_sha256 });
        Ok(self)
    }

    /// The fixed vtsls SEA embeds its TypeScript server and standard library.
    /// No Node CLI, tsserver subprocess, project SDK, plugins or disk project.
    /// Supply an independently approved digest and capped cgroup as for Pyright.
    pub fn with_vtsls_task_group(mut self, group: &Path, image_sha256: [u8; 32]) -> io::Result<Self> {
        crate::sandbox::check_rust_analyzer_abi()?;
        if self.kind != ServerKind::Vtsls || !group.is_absolute() {
            return Err(invalid("expected absolute vtsls task group"));
        }
        self.vtsls_image = Some((group.to_owned(), image_sha256));
        Ok(self)
    }

    /// Opt in to the fixed-entrypoint Kotlin image selected by the operator's
    /// launcher (built-ins or fwcd). Pin the complete image; no extra classpath.
    pub fn with_kotlin_task_group(mut self, group: &Path, image_sha256: [u8; 32]) -> io::Result<Self> {
        crate::sandbox::check_rust_analyzer_abi()?;
        if self.kind != ServerKind::Kotlin || !group.is_absolute() {
            return Err(invalid("expected absolute Kotlin task group"));
        }
        self.kotlin_image = Some((group.to_owned(), image_sha256));
        Ok(self)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Position {
    pub line: u32,
    /// UTF-16 code units.
    pub character: u32,
}

impl Position {
    /// End position of a bounded UTF-8 snapshot, excluding CR/LF terminators.
    fn document_end(text: &str) -> Self {
        let mut pos = Self { line: 0, character: 0 };
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if matches!(c, '\r' | '\n') {
                if c == '\r' {
                    chars.next_if_eq(&'\n');
                }
                pos.line += 1;
                pos.character = 0;
            } else {
                pos.character += c.len_utf16() as u32;
            }
        }
        pos
    }

    fn param(self) -> io::Result<Value> {
        if self.line > i32::MAX as u32 || self.character > i32::MAX as u32 {
            return Err(invalid("position out of range"));
        }
        Ok(json!({"line":self.line, "character":self.character}))
    }

    fn parse(value: &Value) -> io::Result<Self> {
        let number = |name| {
            value
                .get(name)
                .and_then(Value::as_u64)
                .filter(|n| *n <= i32::MAX as u64)
                .map(|n| n as u32)
                .ok_or_else(|| invalid("invalid position"))
        };
        Ok(Self {
            line: number("line")?,
            character: number("character")?,
        })
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

impl Range {
    fn parse(value: &Value) -> io::Result<Self> {
        let start = Position::parse(&value["start"])?;
        let end = Position::parse(&value["end"])?;
        if (start.line, start.character) > (end.line, end.character) {
            return Err(invalid("reversed range"));
        }
        Ok(Self { start, end })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Location {
    /// A caller-opened relative path, never a server-controlled path or URI.
    pub relative_path: String,
    pub range: Range,
}

#[derive(Debug, PartialEq, Eq)]
pub struct WorkspaceSymbol {
    pub name: String,
    pub kind: u8,
    pub location: Location,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub range: Range,
    /// Untrusted display text; not an instruction, hyperlink, command or patch.
    pub message: String,
}

fn diagnostics(items: &Value) -> io::Result<Vec<Diagnostic>> {
    items
        .as_array()
        .filter(|a| a.len() <= MAX_RESULTS)
        .ok_or_else(|| invalid("invalid diagnostics"))?
        .iter()
        .map(|d| {
            Ok(Diagnostic {
                range: Range::parse(&d["range"])?,
                message: d["message"]
                    .as_str()
                    .filter(|s| s.len() <= 4096)
                    .ok_or_else(|| invalid("invalid message"))?
                    .into(),
            })
        })
        .collect()
}

struct Document {
    path: String,
    version: i32,
    end: Position,
    allow_unversioned: bool,
    diagnostics: Option<Vec<Diagnostic>>,
}

struct Process(Option<Child>);

impl Process {
    fn reap(&mut self) {
        if let Some(mut child) = self.0.take() {
            // SAFETY: this is our child's private process group. Its PID is
            // still owned and unreaped; do not signal the group after wait.
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.reap()
    }
}

/// One initialized server per operator-bound workspace and language. Multiple
/// instances are independent; each admits just one outstanding request at a time.
pub struct LocalLsp {
    child: Process,
    wire: Wire,
    workspace: Workspace,
    docs: BTreeMap<String, Document>,
    virtual_root: String,
    kind: ServerKind,
    deadline: Duration,
    request_id: u64,
    capabilities: Capabilities,
    version: i32,
    has_closed: bool,
    rust_ready: bool,
    fwcd_snapshots: bool,
    python_names: Option<pyright::ModuleNames>,
}

/// One preconfigured session per language. Looking up an unprovisioned kind never
/// starts a process or tries to discover a server in the workspace or PATH.
pub struct LocalLspPool(BTreeMap<ServerKind, LocalLsp>);

impl LocalLspPool {
    pub fn start(configs: &[OperatorServer], root: &Path) -> io::Result<Self> {
        if configs.len() > 5
            || configs
                .iter()
                .enumerate()
                .any(|(i, c)| configs[..i].iter().any(|p| p.kind == c.kind))
        {
            return Err(invalid("duplicate or excessive server configuration"));
        }
        let mut sessions = BTreeMap::new();
        for config in configs {
            sessions.insert(config.kind, LocalLsp::start(config, root)?);
        }
        Ok(Self(sessions))
    }

    pub fn session(&mut self, kind: ServerKind) -> Option<&mut LocalLsp> {
        self.0.get_mut(&kind)
    }

    pub fn shutdown(self) -> io::Result<()> {
        for (_, server) in self.0 {
            server.shutdown()?;
        }
        Ok(())
    }
}

fn virtual_uri(root: &str, relative_path: &str) -> String {
    // Called only for a path validated by Workspace. Preserve the relative
    // layout for servers resolving imports, while escaping URI syntax and UTF-8.
    use std::fmt::Write;
    let mut uri = String::from("file://");
    for b in root.bytes().chain(std::iter::once(b'/')).chain(relative_path.bytes()) {
        if b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'.' | b'_' | b'~') {
            uri.push(char::from(b));
        } else {
            write!(&mut uri, "%{b:02X}").expect("String write");
        }
    }
    uri
}

impl LocalLsp {
    /// Returns only after a confined server replies to `initialize` and the
    /// client sends `initialized`. Failure never retries with a weaker policy.
    /// This handshake is not evidence that project indexing or any query works.
    pub fn start(config: &OperatorServer, workspace_root: &Path) -> io::Result<Self> {
        if config.kind == ServerKind::Pyright && config.pyright_group.is_none() {
            return Err(invalid("Pyright requires its pinned image and bounded task group"));
        }
        if config.kind == ServerKind::Vtsls && config.vtsls_image.is_none() {
            return Err(invalid("vtsls requires its pinned image and bounded task group"));
        }
        if config.kind == ServerKind::Kotlin && config.kotlin_image.is_none() {
            return Err(invalid("Kotlin requires its pinned image and bounded task group"));
        }
        Self::spawn(config, workspace_root, config.kind.args())
    }

    // Not exposed: protocol operations must never be able to supply argv.
    fn spawn(config: &OperatorServer, root: &Path, args: &[&str]) -> io::Result<Self> {
        if !root.is_absolute() {
            return Err(invalid("workspace must be absolute"));
        }
        // Do not turn a symlinked root into a different valid Workspace handle.
        let workspace = Workspace::open(root).map_err(io::Error::other)?;
        let root = root.canonicalize()?;
        let python_names = config.pyright_group.as_ref().map(|p| {
            pyright::ModuleNames::new(&p.state, &root)
        }).transpose()?;
        let virtual_root = if let Some(names) = &python_names {
            names.root.to_str().ok_or_else(|| invalid("non-UTF8 state path"))?.to_owned()
        } else { format!(
            "/eon-lsp-{:016x}",
            getrandom::u64().map_err(|_| io::Error::other("no session nonce"))?
        ) };
        let stubs = config.executable.with_file_name("typeshed-fallback").join("stdlib");
        let mut read_only = config.runtime.clone();
        if config.pyright_group.is_some() {
            if stubs.canonicalize()? != stubs || !stubs.is_dir() {
                return Err(invalid("expected trusted Pyright typeshed installation"));
            }
            read_only.push(stubs.parent().expect("stdlib parent").to_owned());
            read_only.push(stubs.clone());
        }
        read_only.push(config.executable.clone());
        if read_only.iter().any(|p| {
            p.starts_with(&root)
                || root.starts_with(p)
                || p.starts_with(&virtual_root)
                || Path::new(&virtual_root).starts_with(p)
        }) {
            return Err(invalid(
                "server installation and runtime must be disjoint from workspace",
            ));
        }
        let worker_io = if config.kind == ServerKind::RustAnalyzer {
            let group = config.rust_task_group.as_deref()
                .ok_or_else(|| invalid("rust-analyzer requires a bounded task group"))?;
            WorkerIo::RustAnalyzer(group)
        } else if let Some(p) = &config.pyright_group {
            read_only.push(PathBuf::from(&virtual_root));
            WorkerIo::Pyright { group: &p.group, names: Path::new(&virtual_root), stubs: &stubs, digest: &p.image_sha256 }
        } else if let Some((group, digest)) = &config.vtsls_image {
            WorkerIo::Vtsls { group, digest }
        } else if let Some((group, digest)) = &config.kotlin_image {
            WorkerIo::Kotlin { group, digest, workspace: &root, deadline: Instant::now() + config.deadline }
        } else {
            WorkerIo::StdioProtocol
        };
        let mut command = Command::new(&config.executable);
        command.args(args).process_group(0);
        confine_command(
            &mut command,
            &ConfinementPolicy {
                read_only,
                deny_network: true,
            },
            worker_io,
        )
        .map_err(io::Error::other)?;
        let mut child = Process(Some(command.spawn().map_err(|error| {
            io::Error::new(error.kind(), format!("confined LSP launch failed: {error}"))
        })?));
        let c = child.0.as_mut().expect("just spawned");
        let wire = Wire::new(
            c.stdin.take().expect("piped stdin"),
            c.stdout.take().expect("piped stdout"),
        )?;
        let mut client = Self {
            child,
            wire,
            workspace,
            kind: config.kind,
            docs: BTreeMap::new(),
            virtual_root,
            deadline: config.deadline,
            request_id: 0,
            capabilities: Capabilities::default(),
            version: 0,
            has_closed: false,
            rust_ready: false,
            fwcd_snapshots: config.kotlin_image.is_some() && config.executable.file_name()
                == Some("fwcd-kotlin-language-server".as_ref()),
            python_names,
        };
        let end = client.end();
        let mut params = json!({
            "processId":null,"rootUri":null,"workspaceFolders":null,
            "capabilities":{"general":{"positionEncodings":["utf-16"]},
                "workspace":{"applyEdit":false,"configuration":false,"workspaceFolders":false,"symbol":{}},
                "textDocument":{"hover":{"contentFormat":["plaintext"]},"definition":{"linkSupport":true},
                    "references":{},"diagnostic":{},"publishDiagnostics":{"versionSupport":true}}}
        });
        if config.pyright_group.is_some() {
            params["workspaceFolders"] = json!([{"name":"snapshots","uri":virtual_uri(&client.virtual_root, "")}]);
        }
        if config.vtsls_image.is_some() {
            // Only negotiated, on-demand reports can satisfy an edited snapshot.
            params["capabilities"]["textDocument"].as_object_mut().expect("client capabilities")
                .remove("publishDiagnostics");
        }
        if client.kind == ServerKind::RustAnalyzer {
            params["rootUri"] = json!(format!("file://{}", client.virtual_root));
            params["capabilities"]["experimental"] = json!({"serverStatusNotification":true});
            params["initializationOptions"] = json!({
                "linkedProjects":[{"crates":[{"root_module":format!("{}/src/lib.rs", client.virtual_root),
                    "edition":"2021","deps":[],"cfg":[],"is_workspace_member":true}]}],
                "numThreads":1,"cachePriming":{"enable":false},
                "checkOnSave":false,"procMacro":{"enable":false},
                "cargo":{"sysroot":null,"buildScripts":{"enable":false}}
            });
        }
        let init = client.request("initialize", params, end).map_err(|error| {
            io::Error::new(error.kind(), format!("confined LSP did not initialize: {error}"))
        })?;
        client.capabilities = Capabilities::parse(&init["capabilities"])?;
        if config.vtsls_image.is_some() {
            require(matches!(client.capabilities.diagnostics, Diagnostics::Pull { .. }), "expected vtsls pull diagnostics")?;
        }
        if config.kotlin_image.is_some() {
            require(matches!(client.capabilities.diagnostics, Diagnostics::Pull { .. }), "expected Kotlin pull diagnostics")?;
        }
        if client.fwcd_snapshots {
            require(init["serverInfo"]["name"] == "fwcd (EON snapshot)" && init["serverInfo"]["version"] == "1.3.13",
                "expected fwcd 1.3.13 snapshot adapter")?;
        }
        // The pinned SEA installs this request handler unconditionally. Select
        // it in the fixed operator profile: early version-matched pushes can
        // still be pre-analysis empties. Never accept dynamic registration.
        if config.pyright_group.is_some() {
            client.capabilities.diagnostics = Diagnostics::Pull { identifier: None };
        }
        client.notify("initialized", json!({}), end)?;
        if client.kind == ServerKind::RustAnalyzer {
            // Opening during initial crate-model setup can leave overlays in a
            // non-local source root (RA then returns empty diagnostic reports).
            for _ in 0..MAX_MESSAGES {
                if client.rust_ready {
                    break;
                }
                client.receive(end, None)?;
            }
            if !client.rust_ready {
                return Err(invalid("rust-analyzer did not finish initial project setup"));
            }
        }
        Ok(client)
    }

    fn end(&self) -> Instant {
        Instant::now() + self.deadline
    }

    fn connected(&self) -> io::Result<()> {
        if self.child.0.is_none() {
            return Err(io::ErrorKind::NotConnected.into());
        }
        Ok(())
    }

    fn retire(&mut self) {
        self.child.reap();
        // Includes a marker staged by open() before didOpen was delivered.
        self.python_names = None;
    }

    fn send(&mut self, packet: Value, end: Instant) -> io::Result<()> {
        self.connected()?;
        self.wire.send(&packet, end).inspect_err(|_| self.retire())
    }

    fn notify(&mut self, method: &str, params: Value, end: Instant) -> io::Result<()> {
        let mut packet = json!({"jsonrpc":"2.0","method":method});
        if !params.is_null() {
            packet["params"] = params;
        }
        self.send(packet, end)
    }

    fn request(&mut self, method: &str, params: Value, end: Instant) -> io::Result<Value> {
        self.request_id = self
            .request_id
            .checked_add(1)
            .ok_or_else(|| invalid("request id exhausted"))?;
        let mut packet = json!({"jsonrpc":"2.0","id":self.request_id,"method":method});
        if !params.is_null() {
            packet["params"] = params;
        }
        self.send(packet, end)?;
        for _ in 0..MAX_MESSAGES {
            if let Some(reply) = self
                .receive(end, Some(self.request_id))
                .inspect_err(|_| self.retire())?
            {
                if reply.get("error").is_some() {
                    return Err(io::Error::other("LSP request rejected"));
                }
                return Ok(reply["result"].clone());
            }
        }
        self.retire();
        Err(invalid("too many unrelated LSP messages"))
    }

    fn receive(&mut self, end: Instant, expected: Option<u64>) -> io::Result<Option<Value>> {
        let msg = self.wire.recv(end)?;
        if msg["jsonrpc"] != "2.0" {
            return Err(invalid("invalid JSON-RPC version"));
        }
        if let Some(method) = msg.get("method") {
            let method = method.as_str().ok_or_else(|| invalid("invalid method"))?;
            if let Some(id) = msg.get("id") {
                if !id.is_i64() && !id.as_str().is_some_and(|s| s.len() <= 128) {
                    return Err(invalid("invalid server request id"));
                }
                // In particular, never honor workspace/applyEdit, executeCommand,
                // dynamic registration, showDocument or configuration requests.
                self.send(
                    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Unsupported"}}),
                    end,
                )?;
            } else if self.kind == ServerKind::RustAnalyzer && method == "experimental/serverStatus" {
                self.rust_ready = msg["params"]["quiescent"] == true
                    && matches!(msg["params"]["health"].as_str(), Some("ok" | "warning"));
            } else if method == "textDocument/publishDiagnostics"
                && matches!(self.capabilities.diagnostics, Diagnostics::Push)
            {
                let p = &msg["params"];
                if let Some(doc) = p["uri"].as_str().and_then(|u| self.docs.get_mut(u))
                    && p.get("version").map_or(doc.allow_unversioned, |v| v.as_i64() == Some(i64::from(doc.version)))
                {
                    doc.diagnostics = Some(diagnostics(&p["diagnostics"])?);
                }
            }
            return Ok(None);
        }
        if expected.is_none()
            || msg["id"].as_u64() != expected
            || msg.get("result").is_some() == msg.get("error").is_some()
        {
            return Err(invalid("unexpected JSON-RPC response"));
        }
        Ok(Some(msg))
    }

    fn next_version(&mut self) -> io::Result<i32> {
        self.version = self
            .version
            .checked_add(1)
            .ok_or_else(|| invalid("document version exhausted"))?;
        Ok(self.version)
    }

    /// Open a workspace snapshot; no disk-backed project is passed to the server.
    /// Generated file URIs are under an inaccessible virtual root, not on disk.
    pub fn open(&mut self, relative_path: &str) -> io::Result<()> {
        self.connected()?;
        require(self.capabilities.sync != Sync::None, "server does not synchronize open documents")?;
        if self.docs.len() >= MAX_OPEN || self.docs.values().any(|d| d.path == relative_path) {
            return Err(invalid("document already open or session full"));
        }
        let language = self.kind.language(relative_path)?;
        let (text, _) = snapshot(&self.workspace, relative_path)?;
        let uri = virtual_uri(&self.virtual_root, relative_path);
        // fwcd validates the ASCII wire URI, not the shorter on-disk path.
        // didOpen is a notification: once sent it cannot be rejected safely.
        if self.fwcd_snapshots && uri.len() > 4096 {
            return Err(invalid("encoded fwcd document URI exceeds 4096 bytes"));
        }
        let version = self.next_version()?;
        let end = Position::document_end(&text);
        if let Some(names) = self.python_names.as_mut() {
            names.insert(relative_path)?;
            self.module_marker_changed(&uri, 1)?;
        }
        self.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,"languageId":language,"version":version,"text":text
            }}),
            self.end(),
        )?;
        self.docs.insert(
            uri,
            Document {
                path: relative_path.into(),
                end,
                version,
                // After a close this URI might be reused. Do not retain an
                // unbounded history of closed paths to disambiguate late pushes.
                allow_unversioned: !self.has_closed,
                diagnostics: None,
            },
        );
        Ok(())
    }

    /// Change only the server's view of an open document, e.g. to analyze an
    /// in-memory replacement preview. Never save, run, or edit the workspace.
    pub fn change(&mut self, path: &str, text: &str) -> io::Result<()> {
        let uri = self.uri(path)?;
        if text.len() > MAX_DOCUMENT {
            return Err(invalid("document exceeds byte limit"));
        }
        let delta = match self.capabilities.sync {
            Sync::Full => json!({"text": text}),
            Sync::Incremental => json!({"range":{
                "start":{"line":0,"character":0},"end":self.docs[&uri].end.param()?
            },"text":text}),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "server does not accept document changes",
                ));
            }
        };
        let version = self.next_version()?;
        self.notify(
            "textDocument/didChange",
            json!({"textDocument":{"uri":uri,"version":version},"contentChanges":[delta]}),
            self.end(),
        )?;
        let doc = self.docs.get_mut(&uri).expect("opened document");
        doc.version = version;
        doc.end = Position::document_end(text);
        doc.allow_unversioned = false;
        doc.diagnostics = None;
        Ok(())
    }

    /// Release the virtual document, cached diagnostics and open-document slot.
    pub fn close(&mut self, path: &str) -> io::Result<()> {
        let uri = self.uri(path)?;
        self.notify("textDocument/didClose", json!({"textDocument":{"uri":uri}}), self.end())?;
        if let Some(names) = self.python_names.as_mut() {
            // didClose already succeeded. Do not reuse a session whose
            // on-disk catalog cannot be kept consistent with that message.
            names.remove(path).inspect_err(|_| self.retire())?;
            self.module_marker_changed(&uri, 3)?;
        }
        self.docs.remove(&uri);
        self.has_closed = true;
        Ok(())
    }

    fn module_marker_changed(&mut self, uri: &str, change: u8) -> io::Result<()> {
        // A notification about our own marker mutation, not a filesystem
        // watcher or a server-controlled path. Invalidate cached import misses.
        self.notify("workspace/didChangeWatchedFiles", json!({"changes":[{"uri":uri,"type":change}]}), self.end())
    }

    fn uri(&self, path: &str) -> io::Result<String> {
        self.docs
            .iter()
            .find(|(_, d)| d.path == path)
            .map(|(u, _)| u.clone())
            .ok_or_else(|| invalid("document not open"))
    }

    fn at(&self, path: &str, pos: Position) -> io::Result<Value> {
        Ok(json!({"textDocument":{"uri":self.uri(path)?}, "position":pos.param()?}))
    }

    fn location(&self, uri: &Value, range: &Value) -> io::Result<Location> {
        let doc = uri
            .as_str()
            .and_then(|s| self.docs.get(s))
            .ok_or_else(|| invalid("location outside open documents"))?;
        Ok(Location {
            relative_path: doc.path.clone(),
            range: Range::parse(range)?,
        })
    }

    fn locations(&self, value: &Value, links: bool) -> io::Result<Vec<Location>> {
        let values = match value {
            Value::Null => return Ok(vec![]),
            Value::Array(a) if a.len() <= MAX_RESULTS => a.as_slice(),
            Value::Object(_) => std::slice::from_ref(value),
            _ => return Err(invalid("invalid location result")),
        };
        values
            .iter()
            .map(|v| {
                if links && v.get("targetUri").is_some() {
                    self.location(&v["targetUri"], &v["targetSelectionRange"])
                } else {
                    self.location(&v["uri"], &v["range"])
                }
            })
            .collect()
    }

    pub fn goto_definition(&mut self, path: &str, pos: Position) -> io::Result<Vec<Location>> {
        require(self.capabilities.definition, "no definition provider")?;
        let value = self.request("textDocument/definition", self.at(path, pos)?, self.end())?;
        self.locations(&value, true)
    }

    pub fn find_references(&mut self, path: &str, pos: Position) -> io::Result<Vec<Location>> {
        require(self.capabilities.references, "no references provider")?;
        let mut params = self.at(path, pos)?;
        params["context"] = json!({"includeDeclaration":true});
        let value = self.request("textDocument/references", params, self.end())?;
        self.locations(&value, false)
    }

    /// Search the server's index; only locations in caller-opened documents
    /// are admissible. No symbol-resolution requests or external URI reads.
    pub fn workspace_symbols(&mut self, query: &str) -> io::Result<Vec<WorkspaceSymbol>> {
        require(self.capabilities.symbols, "no workspace symbol provider")?;
        if query.len() > 256 || query.trim().is_empty() {
            return Err(invalid("invalid symbol query"));
        }
        let value = self.request("workspace/symbol", json!({"query":query}), self.end())?;
        if value.is_null() {
            return Ok(vec![]);
        }
        value
            .as_array()
            .filter(|a| a.len() <= MAX_RESULTS)
            .ok_or_else(|| invalid("invalid workspace symbol result"))?
            .iter()
            .map(|s| {
                let name = s["name"]
                    .as_str()
                    .filter(|s| s.len() <= 1024)
                    .ok_or_else(|| invalid("invalid symbol name"))?;
                let kind = s["kind"]
                    .as_u64()
                    .filter(|n| (1..=26).contains(n))
                    .ok_or_else(|| invalid("invalid symbol kind"))?;
                let loc = &s["location"];
                Ok(WorkspaceSymbol {
                    name: name.into(),
                    kind: kind as u8,
                    location: self.location(&loc["uri"], &loc["range"])?,
                })
            })
            .collect()
    }

    /// Display text only. Never render server output as active HTML or follow links.
    pub fn hover_type(&mut self, path: &str, pos: Position) -> io::Result<Option<String>> {
        require(self.capabilities.hover, "no hover provider")?;
        let value = self.request("textDocument/hover", self.at(path, pos)?, self.end())?;
        if value.is_null() {
            return Ok(None);
        }
        let content = &value["contents"];
        let parts = match content {
            Value::Array(a) if a.len() <= MAX_RESULTS => a.as_slice(),
            _ => std::slice::from_ref(content),
        };
        let mut text = String::new();
        for (i, part) in parts.iter().enumerate() {
            let s = part
                .as_str()
                .or_else(|| part["value"].as_str())
                .ok_or_else(|| invalid("unsupported hover content"))?;
            if text.len() + s.len() + usize::from(i != 0) > 8192 {
                return Err(invalid("hover content exceeds byte limit"));
            }
            if i != 0 {
                text.push('\n');
            }
            text.push_str(s);
        }
        Ok(Some(text))
    }

    /// Prefer LSP 3.17 pull diagnostics when advertised. Otherwise consume push
    /// diagnostics, with the same deadline. A changed document, or any document
    /// opened after a close, requires versioned pushes so stale diagnostics
    /// cannot be mistaken for diagnostics for the new text.
    pub fn diagnostics(&mut self, path: &str) -> io::Result<Vec<Diagnostic>> {
        let uri = self.uri(path)?;
        let end = self.end();
        if let Diagnostics::Pull { identifier } = &self.capabilities.diagnostics {
            let mut params = json!({"textDocument":{"uri":uri}});
            if let Some(id) = identifier {
                params["identifier"] = json!(id);
            }
            let version = self.docs[&uri].version;
            if self.fwcd_snapshots {
                params["x-eon-version"] = json!(version);
            }
            let value = self.request("textDocument/diagnostic", params, end)?;
            if self.fwcd_snapshots && value["x-eon-version"].as_i64() != Some(i64::from(version)) {
                return Err(invalid("stale fwcd snapshot diagnostic"));
            }
            if value["kind"] != "full" {
                return Err(invalid("expected full diagnostic report"));
            }
            return diagnostics(&value["items"]);
        }
        self.connected()?;
        for consumed in 0..=MAX_MESSAGES {
            if !self.wire.readable(end).inspect_err(|_| self.retire())?
                && let Some(items) = &self.docs[&uri].diagnostics
            {
                return Ok(items.clone());
            }
            if consumed == MAX_MESSAGES {
                break;
            }
            self.receive(end, None).inspect_err(|_| self.retire())?;
        }
        self.retire();
        Err(invalid("diagnostic message budget exhausted"))
    }

    /// Complete the LSP shutdown/exit handshake. If the server ignores either
    /// step the process group is terminated and the direct child reaped. Drop
    /// provides the same forced cleanup when no graceful shutdown is requested.
    pub fn shutdown(mut self) -> io::Result<()> {
        let end = self.end();
        if !self.request("shutdown", Value::Null, end)?.is_null() {
            return Err(invalid("invalid shutdown response"));
        }
        self.notify("exit", Value::Null, end)?;
        // Even a well-behaved server may have outstanding helpers in the group.
        // Observe exit without reaping, so Drop can signal the group before the
        // PID becomes reusable.
        let pid = self.child.0.as_ref().expect("live server").id() as libc::pid_t;
        loop {
            if Instant::now() >= end {
                return Err(timeout());
            }
            // SAFETY: waitid writes a valid siginfo_t; WNOWAIT keeps ownership of pid.
            let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
            if unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as _,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            } < 0
            {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            if unsafe { info.si_pid() } == pid {
                return if info.si_code == libc::CLD_EXITED && unsafe { info.si_status() } == 0 {
                    Ok(())
                } else {
                    Err(io::Error::other("server exited unsuccessfully"))
                };
            }
            std::thread::sleep(Duration::from_millis(1).min(end.saturating_duration_since(Instant::now())));
        }
    }
}

#[cfg(test)]
mod lsp_and_patcher_tests;
