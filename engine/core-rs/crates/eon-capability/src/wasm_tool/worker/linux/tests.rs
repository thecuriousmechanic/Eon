use super::*;

// Trusted native supervisor fixtures; never arguments accepted by dispatch.
fn peer(script: &str) -> io::Result<(WorkerChild, UnixStream)> {
    let exe = File::open("/bin/sh")?;
    let (reader, writer) = UnixStream::pair()?;
    let child = spawn(&exe, &["-c", script], request(&[], None)?, writer)?;
    Ok((child, reader))
}

fn assert_reaped(pid: Pid) {
    assert_eq!(
        process::waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG
        )
        .unwrap_err(),
        rustix::io::Errno::CHILD
    );
    assert_eq!(
        process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH)
    );
}

#[test]
fn stalled_io_and_eof_without_exit_share_one_deadline_and_reap() {
    for script in [
        "exec /bin/sleep 30",
        "printf 'valid\\n'; exec 1>&-; exec /bin/sleep 30",
    ] {
        let (child, reader) = peer(script).unwrap();
        let pid = child.pid();
        let deadline = Instant::now() + Duration::from_millis(100);
        let error = read_reply(child, reader, deadline, 64).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(Instant::now() < deadline + Duration::from_secs(3));
        assert_reaped(pid);
    }
}

#[test]
fn reject_oversized_and_unsuccessful_peers_before_accepting_output() {
    for (script, reason) in [
        (
            "printf '%100s' x; exec /bin/sleep 30",
            "oversized worker response",
        ),
        (
            "printf 'valid\\n'; exit 1",
            "worker did not exit successfully",
        ),
    ] {
        let (child, reader) = peer(script).unwrap();
        let pid = child.pid();
        let error =
            read_reply(child, reader, Instant::now() + Duration::from_secs(5), 64).unwrap_err();
        assert_eq!(error.to_string(), reason);
        assert_reaped(pid);
    }
}

#[test]
fn compiler_limits_are_installed_before_exec() {
    let (child, reader) =
        peer("ulimit -H -t; ulimit -H -v; ulimit -H -c; ulimit -S -t; ulimit -S -v").unwrap();
    let pid = child.pid();
    let reply = read_reply(child, reader, Instant::now() + Duration::from_secs(5), 128).unwrap();
    let limits: Vec<u64> = std::str::from_utf8(&reply)
        .unwrap()
        .lines()
        .map(|s| s.parse().unwrap())
        .collect();
    assert_eq!(limits.len(), 5);
    assert!(limits[0] <= CPU_SECONDS && limits[1] <= ADDRESS_SPACE / 1024);
    assert_eq!(limits[2], 0);
    assert_eq!(limits[..2], limits[3..]);
    assert_reaped(pid);
}

#[test]
fn auto_reaping_hosts_cannot_launch_workers() {
    const MARKER: &str = "EON_TEST_WASM_SIGCHLD";
    if std::env::var_os(MARKER).is_none() {
        let name = "wasm_tool::worker::linux::tests::auto_reaping_hosts_cannot_launch_workers";
        assert!(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", name])
                .env(MARKER, "1")
                .status()
                .unwrap()
                .success()
        );
        return;
    }
    // Change the process-wide disposition only in this isolated test host.
    for (handler, flags) in [(libc::SIG_IGN, 0), (libc::SIG_DFL, libc::SA_NOCLDWAIT)] {
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            libc::sigemptyset(&mut action.sa_mask);
            action.sa_sigaction = handler;
            action.sa_flags = flags;
            assert_eq!(
                libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()),
                0
            );
        }
        let error = peer("exit 0").err().expect("auto-reaping worker launch");
        assert_eq!(error.to_string(), "Wasm workers require waitable children");
    }
}
