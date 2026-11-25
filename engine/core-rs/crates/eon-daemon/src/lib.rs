//! Local daemon over a Unix domain socket (docs/implementation/05, section 5).
//!
//! The socket is created with mode 0600 and owned by the daemon's user. Each connection is checked against the
//! daemon's effective UID with `SO_PEERCRED`, so another local user cannot read journal state even if file permissions
//! are changed later. Protocol: one request line, one response, then close.

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::{Duration, Instant};

use eon_cli::{read_status, render_text};
use eon_foundation::owned_dir::OwnedDirectory;
use eon_kernel::recovery::LocalExecutionRecoveryWorker;
use eon_primitives::{EonError, EonResult};
use socket2::{Domain, SockAddr, Socket, Type};

const MAX_REQUEST: usize = 128;
const MAX_RESPONSE: usize = 1024;
const IO_BUDGET: Duration = Duration::from_secs(2);

fn io_err(context: &str, e: std::io::Error) -> EonError {
    EonError::OperationFailed {
        operation: format!("daemon.{context}"),
        reason: e.to_string(),
    }
}

fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

fn metadata(path: &Path) -> io::Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        other => other.map(Some),
    }
}

/// Bind in an owned runtime directory not writable by other users. A hard-link witness
/// pins the socket inode (preventing inode reuse); the startup lock serializes bootstrap.
/// Missing or mismatched evidence requires operator cleanup, never speculative unlink.
pub fn bind(path: &Path) -> EonResult<UnixListener> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let dir = OwnedDirectory::open(parent, false).map_err(|e| io_err("runtime_dir", e))?;
    let name = path
        .file_name()
        .ok_or_else(|| EonError::ValidationError("missing socket name".into()))?;
    bind_pinned(&dir, name)
}

fn bind_pinned(dir: &OwnedDirectory, name: &std::ffi::OsStr) -> EonResult<UnixListener> {
    let mut lock_name = name.to_owned();
    lock_name.push(".bind-lock");
    let gate = dir
        .lock_file(&lock_name)
        .map_err(|e| io_err("bind_lock", e))?;
    gate.try_lock().map_err(|e| io_err("bind_lock", e.into()))?;
    let pinned = dir.path().join(name);
    let path = pinned.as_path();
    let witness = path.with_added_extension("owned");
    let present = metadata(path).map_err(|e| io_err("socket_meta", e))?;
    let owned = metadata(&witness).map_err(|e| io_err("witness_meta", e))?;
    match (present, owned) {
        (None, None) => {}
        (Some(a), Some(b))
            if a.file_type().is_socket()
                && a.uid() == euid()
                && (a.dev(), a.ino()) == (b.dev(), b.ino()) =>
        {
            let probe =
                Socket::new(Domain::UNIX, Type::STREAM, None).map_err(|e| io_err("probe", e))?;
            probe
                .set_nonblocking(true)
                .map_err(|e| io_err("probe", e))?;
            let addr = SockAddr::unix(path).map_err(|e| io_err("probe", e))?;
            if !matches!(probe.connect(&addr), Err(e) if e.kind() == io::ErrorKind::ConnectionRefused)
            {
                return Err(EonError::ValidationError(
                    "socket is live or cannot be proven stale".into(),
                ));
            }
            fs::remove_file(path).map_err(|e| io_err("remove_stale", e))?;
            fs::remove_file(&witness).map_err(|e| io_err("remove_witness", e))?;
        }
        _ => {
            return Err(EonError::ValidationError(
                "refusing to replace unverified socket or witness".into(),
            ));
        }
    }
    let listener = UnixListener::bind(path).map_err(|e| io_err("bind", e))?;
    let node = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| io_err("pin_socket", e))?;
    let meta = node.metadata().map_err(|e| io_err("socket_meta", e))?;
    if !meta.file_type().is_socket() || meta.uid() != euid() || meta.nlink() != 1 {
        return Err(EonError::SecurityViolation(
            "unexpected socket inode".into(),
        ));
    }
    fs::set_permissions(
        format!("/proc/self/fd/{}", node.as_raw_fd()),
        fs::Permissions::from_mode(0o600),
    )
    .map_err(|e| io_err("chmod", e))?;
    fs::hard_link(path, witness).map_err(|e| io_err("socket_witness", e))?;
    Ok(listener)
}

/// UID of the process on the other end, read with SO_PEERCRED. std keeps `peer_cred` unstable.
fn peer_uid(stream: &UnixStream) -> std::io::Result<u32> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` are valid for the duration of the call and sized for `ucred`.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(cred.uid)
}

/// A total deadline, not just an idle timeout: trickling bytes must not reserve the server.
fn read_bounded(
    stream: &mut UnixStream,
    limit: usize,
    line: bool,
    budget: Duration,
) -> io::Result<Vec<u8>> {
    let deadline = Instant::now() + budget;
    let mut bytes = Vec::new();
    let mut buf = [0; 256];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::from(io::ErrorKind::TimedOut));
        }
        stream.set_read_timeout(Some(remaining))?;
        let n = stream.read(&mut buf)?;
        if n == 0 {
            return if line {
                Err(io::Error::from(io::ErrorKind::UnexpectedEof))
            } else {
                Ok(bytes)
            };
        }
        let end = if line {
            buf[..n].iter().position(|b| *b == b'\n')
        } else {
            None
        };
        let take = end.map_or(n, |i| i + 1);
        if bytes.len() + take > limit {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        bytes.extend_from_slice(&buf[..take]);
        if end.is_some() {
            return Ok(bytes);
        }
    }
}

/// Authenticate against the effective UID, *not* pathname metadata or a caller-supplied UID.
pub fn handle_connection(mut stream: UnixStream, journal_dir: &Path) -> EonResult<()> {
    let peer = peer_uid(&stream).map_err(|e| io_err("peer_cred", e))?;
    stream
        .set_write_timeout(Some(IO_BUDGET))
        .map_err(|e| io_err("timeout", e))?;
    if peer != euid() {
        stream
            .write_all(b"ERR unauthorized\n")
            .map_err(|e| io_err("write", e))?;
        return Err(EonError::SecurityViolation(format!(
            "refused peer uid {peer}"
        )));
    }
    let request =
        read_bounded(&mut stream, MAX_REQUEST, true, IO_BUDGET).map_err(|e| io_err("read", e))?;
    let response = match request.trim_ascii() {
        b"STATUS" => match read_status(journal_dir) {
            Ok(status) => render_text(&status),
            Err(_) => "ERR journal unavailable\n".to_string(),
        },
        _ => "ERR unknown command\n".to_string(),
    };
    if response.len() > MAX_RESPONSE {
        return Err(EonError::ValidationError(
            "status response too large".into(),
        ));
    }
    stream
        .write_all(response.as_bytes())
        .map_err(|e| io_err("write", e))
}

/// Accepts `count` connections sequentially. Returns the first handler error, if any, after serving all of them.
pub fn serve(listener: &UnixListener, journal_dir: &Path, count: usize) -> EonResult<()> {
    let mut first_error = None;
    for _ in 0..count {
        let (stream, _) = listener.accept().map_err(|e| io_err("accept", e))?;
        if let Err(e) = handle_connection(stream, journal_dir) {
            first_error.get_or_insert(e);
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// Open the kernel journal and finish reconciliation before accepting work. The returned
/// owner keeps the exclusive writer lock until it (or the resumed scheduler) shuts down.
pub fn boot(journal_dir: &Path) -> EonResult<LocalExecutionRecoveryWorker> {
    LocalExecutionRecoveryWorker::boot(journal_dir)
}

/// Production entry point. Binding the socket after boot prevents a failed or competing
/// boot for the same journal from unlinking the live daemon's socket.
pub fn run(journal_dir: &Path, socket: &Path) -> EonResult<()> {
    let _owner = boot(journal_dir)?;
    let listener = bind(socket)?;
    for stream in listener.incoming() {
        let stream = stream.map_err(|e| io_err("accept", e))?;
        if let Err(e) = handle_connection(stream, journal_dir) {
            eprintln!("eon-daemon: {e}");
        }
    }
    Ok(())
}

/// Client side: sends one command and returns the full response.
pub fn request(path: &Path, command: &str) -> EonResult<String> {
    if command.len() >= MAX_REQUEST || command.contains(['\n', '\r']) {
        return Err(EonError::ValidationError("invalid command frame".into()));
    }
    let client = Socket::new(Domain::UNIX, Type::STREAM, None).map_err(|e| io_err("connect", e))?;
    client
        .connect_timeout(
            &SockAddr::unix(path).map_err(|e| io_err("connect", e))?,
            IO_BUDGET,
        )
        .map_err(|e| io_err("connect", e))?;
    let mut stream = UnixStream::from(std::os::fd::OwnedFd::from(client));
    if peer_uid(&stream).map_err(|e| io_err("peer_cred", e))? != euid() {
        return Err(EonError::SecurityViolation("refused foreign daemon".into()));
    }
    stream
        .set_write_timeout(Some(IO_BUDGET))
        .map_err(|e| io_err("timeout", e))?;
    stream
        .write_all(format!("{command}\n").as_bytes())
        .map_err(|e| io_err("send", e))?;
    let response =
        read_bounded(&mut stream, MAX_RESPONSE, false, IO_BUDGET).map_err(|e| io_err("recv", e))?;
    String::from_utf8(response)
        .map_err(|_| EonError::ValidationError("invalid response encoding".into()))
}

#[cfg(test)]
mod tests {
    use std::thread;

    use eon_foundation::journal::JournalWriter;

    use super::*;

    fn runtime_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    fn journal_with(frames: usize) -> tempfile::TempDir {
        let dir = runtime_dir();
        let mut writer = JournalWriter::open(dir.path()).unwrap();
        for i in 0..frames {
            writer.append_batch(&[format!("e{i}").as_bytes()]).unwrap();
        }
        dir
    }

    #[test]
    fn status_round_trip_over_the_socket() {
        let journal = journal_with(5);
        let runtime = runtime_dir();
        let sock = runtime.path().join("eon.sock");
        let listener = bind(&sock).unwrap();
        let journal_path = journal.path().to_path_buf();
        let server = thread::spawn(move || serve(&listener, &journal_path, 1));

        let response = request(&sock, "STATUS").unwrap();
        server.join().unwrap().unwrap();
        assert!(response.contains("frames: 5"), "response: {response}");
        assert!(response.contains("last durable seq: 5"));
    }

    #[test]
    fn socket_is_only_accessible_to_its_owner() {
        let runtime = runtime_dir();
        let sock = runtime.path().join("eon.sock");
        let _listener = bind(&sock).unwrap();
        let mode = fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn unknown_commands_are_rejected_without_touching_the_journal() {
        let journal = journal_with(1);
        let runtime = runtime_dir();
        let sock = runtime.path().join("eon.sock");
        let listener = bind(&sock).unwrap();
        let journal_path = journal.path().to_path_buf();
        let server = thread::spawn(move || serve(&listener, &journal_path, 1));
        let response = request(&sock, "APPEND evil").unwrap();
        server.join().unwrap().unwrap();
        assert_eq!(response, "ERR unknown command\n");
    }

    #[test]
    fn peer_uid_reports_the_connecting_process_uid() {
        let runtime = runtime_dir();
        let sock = runtime.path().join("eon.sock");
        let listener = bind(&sock).unwrap();
        let client = UnixStream::connect(&sock).unwrap();
        let (server_side, _) = listener.accept().unwrap();
        let our_uid = fs::metadata(&sock).unwrap().uid();
        assert_eq!(peer_uid(&server_side).unwrap(), our_uid);
        drop(client);
    }

    #[test]
    fn a_regular_file_at_the_socket_path_is_never_deleted() {
        let runtime = runtime_dir();
        let path = runtime.path().join("eon.sock");
        fs::write(&path, b"precious").unwrap();
        assert!(bind(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"precious");
    }

    #[test]
    fn a_stale_socket_from_a_previous_run_is_replaced() {
        let runtime = runtime_dir();
        let path = runtime.path().join("eon.sock");
        drop(bind(&path).unwrap());
        assert!(bind(&path).is_ok());
    }

    #[test]
    fn preserve_live_and_foreign_sockets_even_when_a_stale_witness_exists() {
        let runtime = runtime_dir();
        let path = runtime.path().join("eon.sock");
        let live = bind(&path).unwrap();
        let inode = fs::metadata(&path).unwrap().ino();
        assert!(bind(&path).is_err());
        assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
        drop(live);
        fs::remove_file(&path).unwrap();
        let foreign = UnixListener::bind(&path).unwrap();
        let foreign_inode = fs::metadata(&path).unwrap().ino();
        assert!(bind(&path).is_err());
        assert!(UnixStream::connect(&path).is_ok());
        drop(foreign);
        assert!(
            bind(&path).is_err(),
            "even a stale foreign socket must be preserved"
        );
        assert_eq!(fs::metadata(&path).unwrap().ino(), foreign_inode);
    }

    #[test]
    fn refuse_socket_in_shared_writable_directory() {
        let runtime = runtime_dir();
        fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o777)).unwrap();
        assert!(bind(&runtime.path().join("s")).is_err());
        assert_eq!(fs::read_dir(runtime.path()).unwrap().count(), 0);
    }

    #[test]
    fn parent_name_replacement_cannot_redirect_binding_chmod_or_stale_cleanup() {
        use std::os::unix::fs::symlink;
        let root = runtime_dir();
        let nominal = root.path().join("nominal");
        fs::create_dir(&nominal).unwrap();
        fs::set_permissions(&nominal, fs::Permissions::from_mode(0o700)).unwrap();
        let held = OwnedDirectory::open(&nominal, false).unwrap();
        drop(bind(&nominal.join("s")).unwrap());
        let moved = root.path().join("moved");
        fs::rename(&nominal, &moved).unwrap();
        let other = runtime_dir();
        let sentinel = other.path().join("s");
        fs::write(&sentinel, b"preserve").unwrap();
        fs::set_permissions(&sentinel, fs::Permissions::from_mode(0o640)).unwrap();
        symlink(other.path(), &nominal).unwrap();
        assert!(bind(&nominal.join("s")).is_err());
        let _listener = bind_pinned(&held, std::ffi::OsStr::new("s")).unwrap();
        assert!(UnixStream::connect(moved.join("s")).is_ok());
        assert_eq!(fs::read(&sentinel).unwrap(), b"preserve");
        assert_eq!(fs::metadata(&sentinel).unwrap().mode() & 0o777, 0o640);
        assert_eq!(fs::read_dir(other.path()).unwrap().count(), 1);
    }

    #[test]
    fn private_socket_leaf_beneath_mutable_ancestor_is_refused() {
        let root = runtime_dir();
        let shared = root.path().join("shared");
        let private = shared.join("private");
        fs::create_dir_all(&private).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).unwrap();
        fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(bind(&private.join("s")).is_err());
        assert_eq!(fs::read_dir(&private).unwrap().count(), 0);
    }

    #[test]
    fn authorized_connection_does_not_depend_on_any_socket_pathname() {
        let journal = journal_with(1);
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(b"STATUS\n").unwrap();
        handle_connection(server, journal.path()).unwrap();
        let mut reply = String::new();
        client.read_to_string(&mut reply).unwrap();
        assert!(reply.contains("last durable seq: 1"));
    }

    #[test]
    fn reject_oversized_and_unterminated_requests() {
        let journal = journal_with(1);
        for bytes in [vec![b'a'; MAX_REQUEST + 1], b"STATUS".to_vec()] {
            let (mut client, server) = UnixStream::pair().unwrap();
            client.write_all(&bytes).unwrap();
            client.shutdown(std::net::Shutdown::Write).unwrap();
            assert!(handle_connection(server, journal.path()).is_err());
            let mut reply = String::new();
            client.read_to_string(&mut reply).unwrap();
            assert!(reply.is_empty());
        }
    }

    #[test]
    fn client_rejects_long_responses_and_multiline_commands() {
        let runtime = runtime_dir();
        let sock = runtime.path().join("s");
        let listener = bind(&sock).unwrap();
        assert!(request(&sock, "STATUS\nSTATUS").is_err());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert_eq!(
                read_bounded(&mut stream, MAX_REQUEST, true, IO_BUDGET).unwrap(),
                b"STATUS\n"
            );
            stream.write_all(&[b'z'; MAX_RESPONSE + 1]).unwrap();
        });
        assert!(request(&sock, "STATUS").is_err());
        server.join().unwrap();
    }

    #[test]
    fn trickled_bytes_do_not_extend_the_read_deadline() {
        let (mut reader, mut trickle) = UnixStream::pair().unwrap();
        let writer = thread::spawn(move || {
            for byte in b"abcdefghijkl\n" {
                if trickle.write_all(&[*byte]).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
        let result = read_bounded(&mut reader, MAX_REQUEST, true, Duration::from_millis(50));
        drop(reader);
        writer.join().unwrap();
        assert!(matches!(
            result.unwrap_err().kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ));
    }
}
