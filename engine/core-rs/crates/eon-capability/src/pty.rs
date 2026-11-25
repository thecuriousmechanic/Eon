//! Host-only native stdin filters, not a shell or workspace command runner.
//! The host supplies the identity and workflow from its dispatch context, never
//! from tool arguments. Use one provider per host; call `close_owner` on workflow
//! teardown. Session IDs alone confer no authority.
//! The host must leave this provider responsible for waiting on its own children.
//! Linux uses pinned GNU tools; Android x86_64 uses the fixed, signed-APK
//! companion built by `provision/android-pty/build-worker.sh`. Both use devpts.
//! The optional `portable-pty-allocator` selects upstream allocation on
//! provisioned x86_64 glibc Linux. See `provision/portable-pty/README.md`.
#[cfg(all(
    feature = "portable-pty-allocator",
    not(all(target_os = "linux", target_env = "gnu", target_arch = "x86_64"))
))]
compile_error!(
    "portable-pty-allocator requires x86_64 glibc Linux; use the default secured devpts provider on other supported targets"
);
mod native;
#[cfg(target_os = "linux")]
mod process;
#[cfg(test)]
mod pty_tests;

#[cfg(target_os = "linux")]
pub use process::{
    MAX_PROCESS_INPUT, MAX_PROCESS_OUTPUT, PROCESS_TIMEOUT, ProcessMode, ProcessOutput,
    ProcessProvider,
};

use std::collections::{BTreeMap, VecDeque, btree_map::Entry};
use std::io::{self, ErrorKind, Read, Write};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use eon_foundation::identity::{Identity, IdentityAuthority};
use eon_foundation::permissions::Permissions;
use eon_primitives::{IdentityId, SessionId, WorkflowId};
use native::{Programs, Terminal};

pub const MAX_SESSIONS: usize = 8;
pub const MAX_WRITE: usize = 4096;
pub const MAX_READ: usize = 16 * 1024;
pub const MAX_RING_LINES: usize = 10_000;
pub const MAX_RING_BYTES: usize = 256 * 1024;
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

fn authorize(authority: &IdentityAuthority, caller: &Identity) -> io::Result<()> {
    authority
        .authorize(caller, Permissions::EXECUTE_PROCESS)
        .map_err(|_| io::Error::from(ErrorKind::PermissionDenied))
}

/// Trusted dispatch context, not a deserializable claim.
pub struct ProcessOwner {
    pub caller: Identity,
    pub workflow: WorkflowId,
}

/// Closed set of byte transforms in the C locale. Input is always stdin.
#[derive(Clone, Copy, Debug)]
pub enum StreamFilter {
    AsciiUppercase,
    /// Collapse each run of ASCII spaces to one, including across PTY writes.
    SqueezeSpaces,
}

#[cfg(target_os = "linux")]
impl StreamFilter {
    fn args(self) -> &'static [&'static str] {
        match self {
            Self::AsciiUppercase => &["a-z", "A-Z"],
            Self::SqueezeSpaces => &["-s", " "],
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum PtyMode {
    Loopback,
    /// Line-oriented native filter. End a request with LF for prompt output;
    /// a partial line may stay in the child's bounded stdio buffer.
    Filter(StreamFilter),
}

impl PtyMode {
    fn probe(self) -> (&'static [u8], &'static [u8]) {
        // LF flushes Linux tr and resets squeeze state on both backends.
        match self {
            Self::Loopback => (b"?", b"?"),
            Self::Filter(StreamFilter::AsciiUppercase) => (b"a\n", b"A\n"),
            Self::Filter(StreamFilter::SqueezeSpaces) => (b"  \n", b" \n"),
        }
    }
}

/// Opaque, untrusted output. Never send `bytes` directly to a terminal,
/// logger or markup interpreter. `escaped` produces inert ASCII for plain text UIs.
#[derive(Debug)]
pub struct RingPage {
    /// Absolute byte cursor; greater than the requested cursor after eviction.
    pub start: u64,
    pub next: u64,
    pub bytes: Vec<u8>,
}

impl RingPage {
    pub fn escaped(&self) -> String {
        self.bytes.escape_ascii().to_string()
    }
}

#[derive(Clone, Copy, Debug)]
pub enum PtySignal {
    Interrupt,
    Terminate,
    Resize { rows: u16, cols: u16 },
}

#[derive(Default)]
struct Ring {
    bytes: VecDeque<u8>,
    start: u64,
    newlines: usize,
}

impl Ring {
    fn push(&mut self, data: &[u8]) {
        for &byte in data {
            if self.bytes.len() == MAX_RING_BYTES {
                self.evict_byte();
            }
            self.bytes.push_back(byte);
            self.newlines += usize::from(byte == b'\n');
            // Count an unterminated last line as a line. Bound even a single
            // line of arbitrary length, retaining its most recent suffix.
            while self.newlines + usize::from(byte != b'\n') > MAX_RING_LINES {
                self.evict_byte();
            }
        }
    }

    fn evict_byte(&mut self) {
        self.newlines -= usize::from(self.bytes.pop_front() == Some(b'\n'));
        self.start += 1;
    }

    fn page(&self, cursor: u64, limit: usize) -> io::Result<RingPage> {
        let start = cursor.max(self.start);
        let offset = start - self.start;
        if !(1..=MAX_READ).contains(&limit) || offset > self.bytes.len() as u64 {
            return Err(io::Error::new(ErrorKind::InvalidInput, "invalid PTY page"));
        }
        let bytes: Vec<_> = self
            .bytes
            .range(offset as usize..)
            .take(limit)
            .copied()
            .collect();
        Ok(RingPage {
            start,
            next: start + bytes.len() as u64,
            bytes,
        })
    }
}

struct Session {
    owner: WorkflowId,
    caller: IdentityId,
    terminal: Terminal,
    ring: Ring,
    last_io: Instant,
}

impl Session {
    // At most one page per pass per PTY; no busy writer can monopolize the lock.
    fn drain(&mut self) -> io::Result<()> {
        let mut buf = [0; MAX_READ];
        match self.terminal.master.read(&mut buf) {
            Ok(0) => Err(io::Error::from(ErrorKind::UnexpectedEof)),
            Ok(n) => {
                self.ring.push(&buf[..n]);
                self.last_io = Instant::now();
                Ok(())
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[derive(Default)]
struct State {
    sessions: BTreeMap<SessionId, Session>,
    stopping: bool,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn watch(&self, idle: Duration) {
        let mut state = self.lock();
        while !state.stopping {
            state
                .sessions
                .retain(|_, s| s.last_io.elapsed() < idle && s.drain().is_ok());
            state = if state.sessions.is_empty() {
                self.wake.wait(state).unwrap_or_else(|e| e.into_inner())
            } else {
                self.wake
                    .wait_timeout(state, idle.min(Duration::from_millis(25)))
                    .unwrap_or_else(|e| e.into_inner())
                    .0
            };
        }
        state.sessions.clear(); // Kill and wait before the watchdog returns.
    }
}

pub struct PtyProvider {
    authority: IdentityAuthority,
    programs: Programs,
    shared: Arc<Shared>,
    watchdog: Option<JoinHandle<()>>,
    idle: Duration,
}

impl PtyProvider {
    /// Pin the trusted host installation. Unsupported layouts and missing
    /// kernel protections fail closed; there is no unconfined fallback.
    pub fn new(authority: &IdentityAuthority) -> io::Result<Self> {
        Self::with_timeout(authority, IDLE_TIMEOUT)
    }

    fn with_timeout(authority: &IdentityAuthority, idle: Duration) -> io::Result<Self> {
        let programs = Programs::pin()?;
        let shared = Arc::new(Shared::default());
        let watcher = shared.clone();
        let watchdog = thread::Builder::new()
            .name("eon-pty".into())
            .spawn(move || watcher.watch(idle))?;
        Ok(Self {
            authority: authority.clone(),
            programs,
            shared,
            watchdog: Some(watchdog),
            idle,
        })
    }

    /// `&mut self` serializes admission, but launch never holds the watchdog lock.
    pub fn pty_open(&mut self, owner: &ProcessOwner, mode: PtyMode) -> io::Result<SessionId> {
        authorize(&self.authority, &owner.caller)?;
        if self.shared.lock().sessions.len() >= MAX_SESSIONS {
            return Err(io::Error::new(ErrorKind::ResourceBusy, "PTY limit reached"));
        }
        let terminal = self.programs.launch(mode)?;
        let id = SessionId::new();
        self.shared.lock().sessions.insert(
            id.clone(),
            Session {
                owner: owner.workflow.clone(),
                caller: owner.caller.id().clone(),
                terminal,
                ring: Ring::default(),
                last_io: Instant::now(),
            },
        );
        self.shared.wake.notify_one();
        Ok(id)
    }

    fn access<T>(
        &self,
        owner: &ProcessOwner,
        id: &SessionId,
        op: impl FnOnce(&mut Session) -> io::Result<T>,
    ) -> io::Result<T> {
        authorize(&self.authority, &owner.caller)?;
        let absent = || io::Error::new(ErrorKind::NotFound, "unknown PTY");
        let mut state = self.shared.lock();
        let Entry::Occupied(mut entry) = state.sessions.entry(id.clone()) else {
            return Err(absent());
        };
        if entry.get().owner != owner.workflow || &entry.get().caller != owner.caller.id() {
            return Err(absent());
        }
        if entry.get().last_io.elapsed() >= self.idle {
            entry.remove();
            return Err(absent());
        }
        let result = op(entry.get_mut());
        if let Err(e) = &result
            && !matches!(
                e.kind(),
                ErrorKind::InvalidInput | ErrorKind::WouldBlock | ErrorKind::Interrupted
            )
        {
            entry.remove(); // A failed worker/transport must not outlive the session.
        }
        result
    }

    /// Nonblocking. A short count consumes only that prefix; retry the suffix.
    pub fn pty_write(
        &self,
        owner: &ProcessOwner,
        id: &SessionId,
        data: &[u8],
    ) -> io::Result<usize> {
        self.access(owner, id, |s| {
            if !(1..=MAX_WRITE).contains(&data.len()) {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "invalid PTY write length",
                ));
            }
            let n = s.terminal.master.write(data)?;
            if n == 0 {
                return Err(io::Error::from(ErrorKind::WriteZero));
            }
            s.last_io = Instant::now();
            Ok(n)
        })
    }

    /// Nonblocking byte pagination, including incomplete lines. Reading cached
    /// output is not terminal I/O, even if a caller repeatedly reads a full page.
    pub fn pty_read_ring(
        &self,
        owner: &ProcessOwner,
        id: &SessionId,
        cursor: u64,
        limit: usize,
    ) -> io::Result<RingPage> {
        self.access(owner, id, |s| s.ring.page(cursor, limit))
    }

    /// Only typed signals to this session's own terminal/group, never a caller PID.
    pub fn pty_signal(
        &self,
        owner: &ProcessOwner,
        id: &SessionId,
        signal: PtySignal,
    ) -> io::Result<()> {
        self.access(owner, id, |s| s.terminal.signal(signal))
    }

    /// Trusted host teardown (not a caller operation): revoke and reap all
    /// sessions for the workflow, including those of delegated identities.
    pub fn close_owner(&mut self, owner: &WorkflowId) {
        self.shared.lock().sessions.retain(|_, s| &s.owner != owner);
    }
}

impl Drop for PtyProvider {
    fn drop(&mut self) {
        self.shared.lock().stopping = true;
        self.shared.wake.notify_one();
        if let Some(worker) = self.watchdog.take() {
            let _ = worker.join();
        }
    }
}
