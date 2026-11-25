use super::*;
use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

fn owner(authority: &IdentityAuthority) -> ProcessOwner {
    ProcessOwner {
        caller: authority.user(Permissions::EXECUTE_PROCESS),
        workflow: WorkflowId::new(),
    }
}

fn pid(p: &PtyProvider, id: &SessionId) -> u32 {
    p.shared.lock().sessions[id].terminal.pid()
}

fn dimensions(master: &fs::File) -> (u16, u16) {
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCGWINSZ writes to this winsize for the live master.
    assert_eq!(
        unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCGWINSZ, &mut size) },
        0
    );
    (size.ws_row, size.ws_col)
}

#[cfg(target_os = "linux")]
pub(super) fn assert_child_image(proc: &Path, split: &str, classic: &str) {
    let expected = match fs::symlink_metadata(split) {
        Err(e) if e.kind() == ErrorKind::NotFound => fs::symlink_metadata(classic).unwrap(),
        result => result.unwrap(),
    };
    assert!(
        expected.is_file(),
        "expected a standalone image, not a link"
    );
    let actual = fs::metadata(proc.join("exe")).unwrap();
    assert_eq!(
        (actual.dev(), actual.ino()),
        (expected.dev(), expected.ino())
    );
}

fn until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for PTY");
        thread::sleep(Duration::from_millis(5));
    }
}

fn gone(pid: u32) {
    // Distinguishes reaping from merely signalling a process (zombies still have /proc entries).
    let proc = PathBuf::from(format!("/proc/{pid}"));
    until(|| !proc.exists());
}

#[test]
fn devpts_peer_is_operator_only_before_first_unlock() {
    // Inspect only metadata on our own fresh pair: no second data opens,
    // privilege changes, or timing race are needed to check the admission order.
    let (master, pin) = native::private_locked_pair().unwrap();
    let mut locked: libc::c_int = 0;
    let mut number: libc::c_uint = 0;
    // SAFETY: query ioctls write a single integer each for the pinned master.
    unsafe {
        assert_eq!(
            libc::ioctl(master.as_raw_fd(), native::TIOCGPTLCK, &mut locked),
            0
        );
        assert_eq!(
            libc::ioctl(master.as_raw_fd(), native::TIOCGPTN, &mut number),
            0
        );
    }
    let m = pin.metadata().unwrap();
    assert_eq!(locked, 1);
    assert_eq!(
        rustix::fs::fstatfs(&pin).unwrap().f_type,
        0x1cd1 // DEVPTS_SUPER_MAGIC
    );
    assert_eq!(
        (rustix::fs::major(m.rdev()), rustix::fs::minor(m.rdev())),
        (136, number)
    );
    assert_eq!(m.mode() & 0o7777, 0o600);
    assert_eq!(m.uid(), rustix::process::geteuid().as_raw());
    assert_eq!(m.gid(), rustix::process::getegid().as_raw());
}

fn send(p: &PtyProvider, owner: &ProcessOwner, id: &SessionId, mut bytes: &[u8]) {
    until(|| {
        match p.pty_write(owner, id, &bytes[..bytes.len().min(MAX_WRITE)]) {
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == ErrorKind::WouldBlock => (),
            Err(e) => panic!("PTY write: {e}"),
        }
        bytes.is_empty()
    });
}

#[test]
fn persistent_filters_keep_state_without_interpreting_input() {
    let auth = IdentityAuthority::new();
    let mut p = PtyProvider::new(&auth).unwrap();
    let owner = owner(&auth);
    let id = p
        .pty_open(&owner, PtyMode::Filter(StreamFilter::SqueezeSpaces))
        .unwrap();
    let child = pid(&p, &id);
    let argv = fs::read(format!("/proc/{child}/cmdline")).unwrap();
    #[cfg(target_os = "linux")]
    assert_eq!(argv, b"tr\0-s\0 \0");
    #[cfg(target_os = "android")]
    assert_eq!(argv, b"eon-pty\0squeeze\0");
    // The first user space survives the admission probe. The filter keeps its
    // final-space state even when the next write comes after observing a line.
    send(&p, &owner, &id, b"    alpha   \nleft   ");
    until(|| {
        p.pty_read_ring(&owner, &id, 0, MAX_READ)
            .unwrap()
            .bytes
            .starts_with(b" alpha \n")
    });
    send(&p, &owner, &id, b"   beta\t  \n");
    until(|| {
        p.pty_read_ring(&owner, &id, 0, MAX_READ).unwrap().bytes == b" alpha \nleft beta\t \n"
    });
    let upper = p
        .pty_open(&owner, PtyMode::Filter(StreamFilter::AsciiUppercase))
        .unwrap();
    let upper_pid = pid(&p, &upper);
    let bytes = b"--help; $(literal) /etc/os-release\x00\xff\n";
    send(&p, &owner, &upper, bytes);
    until(|| {
        p.pty_read_ring(&owner, &upper, 0, MAX_READ).unwrap().bytes == bytes.to_ascii_uppercase()
    });
    p.close_owner(&owner.workflow);
    gone(child);
    gone(upper_pid);
}

#[test]
fn filtered_session_requires_host_issuer_grant_workflow_and_exact_identity() {
    let auth = IdentityAuthority::new();
    let mut p = PtyProvider::new(&auth).unwrap();
    let mode = PtyMode::Filter(StreamFilter::AsciiUppercase);
    let caller = auth.user(Permissions::EXECUTE_PROCESS | Permissions::SPAWN_SUBAGENT);
    let owner = ProcessOwner {
        caller: caller.clone(),
        workflow: WorkflowId::new(),
    };
    let id = p.pty_open(&owner, mode).unwrap();
    for (identity, workflow, expected) in [
        (
            caller.delegate(Permissions::EXECUTE_PROCESS).unwrap(),
            owner.workflow.clone(),
            ErrorKind::NotFound,
        ),
        (caller.clone(), WorkflowId::new(), ErrorKind::NotFound),
        (
            caller.delegate(Permissions::NONE).unwrap(),
            owner.workflow.clone(),
            ErrorKind::PermissionDenied,
        ),
        (
            IdentityAuthority::new().system(Permissions::EXECUTE_PROCESS),
            owner.workflow.clone(),
            ErrorKind::PermissionDenied,
        ),
    ] {
        let other = ProcessOwner {
            caller: identity,
            workflow,
        };
        if expected == ErrorKind::PermissionDenied {
            assert_eq!(p.pty_open(&other, mode).unwrap_err().kind(), expected);
        }
        assert_eq!(
            p.pty_read_ring(&other, &id, 0, 1).unwrap_err().kind(),
            expected
        );
        assert_eq!(p.pty_write(&other, &id, b"x").unwrap_err().kind(), expected);
        assert_eq!(
            p.pty_signal(&other, &id, PtySignal::Terminate)
                .unwrap_err()
                .kind(),
            expected
        );
    }
    send(&p, &owner, &id, b"still live\n");
    until(|| p.pty_read_ring(&owner, &id, 0, MAX_READ).unwrap().bytes == b"STILL LIVE\n");
}

#[test]
fn bidirectional_real_pty_owner_checks_paging_and_inert_display() {
    let auth = IdentityAuthority::new();
    let mut p = PtyProvider::new(&auth).unwrap();
    let other = owner(&auth);
    let owner = owner(&auth);
    let id = p.pty_open(&owner, PtyMode::Loopback).unwrap();
    let id2 = p.pty_open(&other, PtyMode::Loopback).unwrap();
    let pid2 = pid(&p, &id2);
    let pid1 = pid(&p, &id);
    assert_ne!(id, id2);
    assert_eq!(
        p.pty_read_ring(&other, &id, 0, 20).unwrap_err().kind(),
        ErrorKind::NotFound
    );
    assert_eq!(
        p.pty_write(&other, &id, b"x").unwrap_err().kind(),
        ErrorKind::NotFound
    );
    assert_eq!(
        p.pty_signal(&other, &id, PtySignal::Terminate)
            .unwrap_err()
            .kind(),
        ErrorKind::NotFound
    );
    assert_eq!(
        p.pty_read_ring(&owner, &id, 1, 1).unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
    assert!(p.pty_read_ring(&owner, &id, 0, MAX_READ + 1).is_err());
    assert!(p.pty_write(&owner, &id, &[0; MAX_WRITE + 1]).is_err());
    assert!(
        p.pty_signal(&owner, &id, PtySignal::Resize { rows: 0, cols: 1 })
            .is_err()
    );
    {
        let state = p.shared.lock();
        for (session, child) in [(&id, pid1), (&id2, pid2)] {
            let master = &state.sessions[session].terminal.master;
            assert_eq!(dimensions(master), (24, 80));
            // SAFETY: read-only queries on our live master. Being a session
            // leader alone does not establish that it owns this terminal.
            unsafe {
                assert_eq!(libc::tcgetsid(master.as_raw_fd()), child as i32);
                assert_eq!(libc::tcgetpgrp(master.as_raw_fd()), child as i32);
            }
        }
    }
    p.pty_signal(
        &owner,
        &id,
        PtySignal::Resize {
            rows: 35,
            cols: 110,
        },
    )
    .unwrap();
    {
        let state = p.shared.lock();
        let master = &state.sessions[&id].terminal.master;
        assert_eq!(dimensions(master), (35, 110));
        assert_eq!(dimensions(&state.sessions[&id2].terminal.master), (24, 80));
        let fd = master.as_raw_fd();
        let mut locked: libc::c_int = 0;
        // SAFETY: TIOCGPTLCK writes an integer for this live master.
        assert_eq!(
            unsafe { libc::ioctl(fd, native::TIOCGPTLCK, &mut locked) },
            0
        );
        assert_eq!(
            locked, 1,
            "the admitted worker needs no further slave opens"
        );
    }

    // Include a terminal escape, a control keystroke, invalid UTF-8 and a
    // partial final line. No shell, line discipline, or UI may reinterpret them.
    let bytes = b"cat /proc/self/environ\n\x1b]0;title\x07\x03\xff\rpartial";
    send(&p, &owner, &id, bytes);
    let mut cursor = 0;
    let mut observed = Vec::new();
    until(|| {
        let page = p.pty_read_ring(&owner, &id, cursor, 7).unwrap();
        assert_eq!(page.start, cursor);
        cursor = page.next;
        assert!(page.escaped().bytes().all(|b| (0x20..=0x7e).contains(&b)));
        observed.extend_from_slice(&page.bytes);
        observed.len() >= bytes.len()
    });
    assert_eq!(observed, bytes);
    assert!(
        p.pty_read_ring(&other, &id2, 0, 1)
            .unwrap()
            .bytes
            .is_empty()
    );
    p.close_owner(&owner.workflow);
    gone(pid1);
    assert!(p.pty_write(&owner, &id, b"revoked").is_err());
    send(&p, &other, &id2, b"live");
    drop(p);
    gone(pid2);
}

#[test]
fn ring_bounds_long_unterminated_output_and_reports_cursor_loss() {
    let mut ring = Ring::default();
    ring.push(&vec![b'a'; MAX_RING_BYTES + 123]);
    assert_eq!(ring.start, 123);
    assert_eq!(ring.bytes.len(), MAX_RING_BYTES);
    assert_eq!(ring.page(0, MAX_READ).unwrap().bytes.len(), MAX_READ);
    ring.push(&[b'\n'; MAX_RING_LINES]);
    // At the line limit the first retained line can still have a byte-limited prefix.
    ring.push(b"x");
    assert_eq!(ring.newlines, MAX_RING_LINES - 1);
    assert_eq!(ring.bytes.len(), MAX_RING_LINES);
    ring.push(b"\n");
    assert_eq!(ring.newlines, MAX_RING_LINES);
    let tail = ring.start + ring.bytes.len() as u64;
    assert_eq!(ring.page(tail - 2, 2).unwrap().bytes, b"x\n");
    assert!(ring.page(u64::MAX, 1).is_err());
    assert!(ring.page(0, 0).is_err());
}

#[test]
fn unattended_output_uses_ten_thousand_line_ring() {
    let auth = IdentityAuthority::new();
    let mut p = PtyProvider::new(&auth).unwrap();
    let owner = owner(&auth);
    let id = p.pty_open(&owner, PtyMode::Loopback).unwrap();
    let data = b"x\n".repeat(MAX_RING_LINES + 20);
    send(&p, &owner, &id, &data);
    until(|| {
        p.pty_read_ring(&owner, &id, data.len() as u64 - 1, 1)
            .is_ok_and(|p| p.bytes == b"\n")
    });
    let oldest = p.pty_read_ring(&owner, &id, 0, 3).unwrap();
    assert_eq!((oldest.start, oldest.next), (40, 43));
    assert_eq!(oldest.bytes, b"x\nx");
}

#[test]
fn capacity_termination_interrupt_and_reuse() {
    let auth = IdentityAuthority::new();
    let owner = owner(&auth);
    let mut p = PtyProvider::new(&auth).unwrap();
    let ids = (0..MAX_SESSIONS)
        .map(|_| p.pty_open(&owner, PtyMode::Loopback).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        p.pty_open(&owner, PtyMode::Loopback).unwrap_err().kind(),
        ErrorKind::ResourceBusy
    );
    for (id, sig) in ids.iter().zip([PtySignal::Interrupt, PtySignal::Terminate]) {
        let pid = pid(&p, id);
        p.pty_signal(&owner, id, sig).unwrap();
        gone(pid);
        assert_eq!(
            p.pty_write(&owner, id, b"gone").unwrap_err().kind(),
            ErrorKind::NotFound
        );
    }
    p.pty_open(&owner, PtyMode::Loopback).unwrap();
}

#[test]
fn idle_watchdog_reaps_without_any_caller_or_lazy_sweep() {
    assert_eq!(IDLE_TIMEOUT, Duration::from_secs(300));
    let auth = IdentityAuthority::new();
    let owner = owner(&auth);
    let mut p = PtyProvider::with_timeout(&auth, Duration::from_millis(150)).unwrap();
    let id = p
        .pty_open(&owner, PtyMode::Filter(StreamFilter::AsciiUppercase))
        .unwrap();
    let pid = pid(&p, &id);
    gone(pid); // No provider API calls until after wait has happened.
    assert_eq!(
        p.pty_read_ring(&owner, &id, 0, 1).unwrap_err().kind(),
        ErrorKind::NotFound
    );
}

#[test]
fn cached_output_invalid_requests_and_resizes_do_not_renew_idle_workers() {
    let auth = IdentityAuthority::new();
    let owner = owner(&auth);
    let mut p = PtyProvider::new(&auth).unwrap();
    let id = p.pty_open(&owner, PtyMode::Loopback).unwrap();
    let child = pid(&p, &id);
    let opened = p.shared.lock().sessions[&id].last_io;
    send(&p, &owner, &id, b"one");
    until(|| p.pty_read_ring(&owner, &id, 0, MAX_READ).unwrap().bytes == b"one");
    let active = p.shared.lock().sessions[&id].last_io;
    assert!(active > opened);
    for cursor in [0, 1, 2, 0, 3] {
        p.pty_read_ring(&owner, &id, cursor, 2).unwrap();
    }
    assert!(p.pty_write(&owner, &id, b"").is_err());
    p.pty_signal(&owner, &id, PtySignal::Resize { rows: 10, cols: 20 })
        .unwrap();
    assert_eq!(p.shared.lock().sessions[&id].last_io, active);
    send(&p, &owner, &id, b"two");
    until(|| p.pty_read_ring(&owner, &id, 3, 3).unwrap().bytes == b"two");
    assert!(p.shared.lock().sessions[&id].last_io > active);

    // Exercise the production timeout and background reaper without a five-minute sleep.
    p.shared.lock().sessions.get_mut(&id).unwrap().last_io = Instant::now() - IDLE_TIMEOUT;
    gone(child);
    assert_eq!(
        p.pty_read_ring(&owner, &id, 0, 1).unwrap_err().kind(),
        ErrorKind::NotFound
    );
}

#[test]
fn child_has_only_pty_stdio_no_environment_no_secret_and_a_private_group() {
    const FD: &str = "EON_TEST_PTY_INHERITED_FD";
    const MARKER: &str = "EON_TEST_PTY_PRIVATE";
    if std::env::var_os(FD).is_none() {
        // Seed an isolated copy of the test host, not the parallel test runner.
        let (host, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(b"private host fixture").unwrap();
        let fd = host.as_raw_fd();
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "--exact",
            "pty::pty_tests::child_has_only_pty_stdio_no_environment_no_secret_and_a_private_group",
        ])
        .env_clear()
        .env(FD, fd.to_string())
        .env(MARKER, "private host fixture")
        .env("PATH", "/no/tools/here");
        // SAFETY: operate only on this fixture fd in the forked test host.
        unsafe {
            cmd.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let result = cmd.output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stdout)
        );
        assert!(String::from_utf8_lossy(&result.stdout).contains("1 passed"));
        return;
    }
    let inherited: i32 = std::env::var(FD).unwrap().parse().unwrap();
    // SAFETY: F_GETFD only inspects the fixture descriptor flags.
    assert_eq!(unsafe { libc::fcntl(inherited, libc::F_GETFD) }, 0);
    assert_eq!(std::env::var(MARKER).unwrap(), "private host fixture");
    let auth = IdentityAuthority::new();
    let mut p = PtyProvider::new(&auth).unwrap();
    let owner = owner(&auth);
    let id = p.pty_open(&owner, PtyMode::Loopback).unwrap();
    let pid = pid(&p, &id);
    let proc = PathBuf::from(format!("/proc/{pid}"));
    assert!(fs::read(proc.join("environ")).unwrap().is_empty());
    #[cfg(target_os = "linux")]
    {
        assert_eq!(fs::read(proc.join("cmdline")).unwrap(), b"cat\0");
        assert_child_image(&proc, "/usr/bin/gnucat", "/usr/bin/cat");
    }
    #[cfg(target_os = "android")]
    {
        assert_eq!(fs::read(proc.join("cmdline")).unwrap(), b"eon-pty\0copy\0");
        let image = fs::read_link(proc.join("exe")).unwrap();
        assert!(
            image.starts_with("/data/app") && image.ends_with("lib/x86_64/libeon_pty_worker.so")
        );
        let running = fs::metadata(proc.join("exe")).unwrap();
        let pinned = p.programs.pinned_image().metadata().unwrap();
        assert_eq!((running.dev(), running.ino()), (pinned.dev(), pinned.ino()));
    }
    assert_eq!(fs::read_link(proc.join("cwd")).unwrap(), PathBuf::from("/"));
    let status = fs::read_to_string(proc.join("status")).unwrap();
    assert!(status.lines().any(|s| s == "NoNewPrivs:\t1"));
    assert!(status.lines().any(|s| s == "Seccomp:\t2"));
    #[cfg(target_os = "android")]
    {
        // A zygote child already has a seccomp filter. Mode 2 alone does not
        // prove that our pinned helper installed its stdio-only allowlist.
        fn count(status: &str) -> u32 {
            status
                .lines()
                .find_map(|s| s.strip_prefix("Seccomp_filters:"))
                .unwrap()
                .trim()
                .parse()
                .unwrap()
        }
        assert_eq!(
            count(&status),
            count(&fs::read_to_string("/proc/thread-self/status").unwrap()) + 1
        );
    }
    // SAFETY: queries on our live child PID.
    assert_eq!(unsafe { libc::getsid(pid as i32) }, pid as i32);
    assert_eq!(unsafe { libc::getpgid(pid as i32) }, pid as i32);
    let fds = proc.join("fd");
    assert_eq!(fs::read_dir(&fds).unwrap().count(), 3);
    let slave = fs::read_link(fds.join("0")).unwrap();
    assert!(slave.starts_with("/dev/pts"));
    let m = fs::metadata(fds.join("0")).unwrap();
    assert_eq!(m.mode() & 0o7777, 0o600);
    assert_eq!(m.uid(), rustix::process::geteuid().as_raw());
    assert_eq!(m.gid(), rustix::process::getegid().as_raw());
    for n in ["1", "2"] {
        assert_eq!(fs::read_link(fds.join(n)).unwrap(), slave);
    }
    p.close_owner(&owner.workflow);
    gone(pid);

    // This isolated host now auto-reaps children. Admission must fail before
    // spawning anything, rather than signalling a potentially reused group ID.
    // SAFETY: this test is running alone in the fresh host process.
    assert_ne!(
        unsafe { libc::signal(libc::SIGCHLD, libc::SIG_IGN) },
        libc::SIG_ERR
    );
    assert!(p.pty_open(&owner, PtyMode::Loopback).is_err());
}
