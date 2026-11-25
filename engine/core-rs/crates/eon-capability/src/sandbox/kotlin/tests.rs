use super::*;
use std::fs;

// This query is needed only by libnet. Exercise the other production BPF
// programs separately; they must still reject it even without Landlock.
#[test]
fn jvm_fd_query_is_not_in_other_lsp_filters() {
    let path = Path::new("/");
    for profile in [
        WorkerIo::StdioProtocol,
        WorkerIo::RustAnalyzer(path),
        WorkerIo::Pyright { group: path, names: path, stubs: path, digest: &[0; 32] },
        WorkerIo::Vtsls { group: path, digest: &[0; 32] },
    ] {
        let filters = worker_filters(profile).unwrap();
        let mut command = Command::new("/usr/bin/true");
        // SAFETY: only install the filters and query the forked child's own limit.
        unsafe {
            command.pre_exec(move || {
                for f in &filters {
                    seccompiler::apply_filter(f).map_err(|_| io::Error::from_raw_os_error(libc::EIO))?;
                }
                denied(libc::syscall(libc::SYS_prlimit64, 0, libc::RLIMIT_NOFILE, 0usize, 0usize), libc::EPERM)?;
                denied(libc::syscall(libc::SYS_seccomp, libc::SECCOMP_SET_MODE_FILTER, 0, 0usize), libc::EPERM)
            });
        }
        assert!(command.status().unwrap().success());
    }
}

#[test]
fn rejects_fifo_before_reading_image() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target").canonicalize().unwrap()
        .join(format!("kotlin-not-an-image-{:016x}", getrandom::u64().unwrap()));
    fs::create_dir(&root).unwrap();
    let program = root.join(IMAGE[0]);
    rustix::fs::mkfifoat(rustix::fs::CWD, &program, rustix::fs::Mode::RUSR).unwrap();
    let policy = ConfinementPolicy { read_only: vec![program.clone()], deny_network: true };
    let io = WorkerIo::Kotlin { group: Path::new("/"), digest: &[0; 32], workspace: Path::new("/eon-project"),
        deadline: std::time::Instant::now() + std::time::Duration::from_secs(1) };
    let err = build_ruleset(&policy, &program, io).err().unwrap();
    fs::remove_dir_all(root).unwrap();
    assert!(err.to_string().contains("regular") || err.to_string().contains("immutable operator-owned image files"), "{err}");
}

fn denied(status: i64, errno: i32) -> io::Result<()> {
    if status == -1 && io::Error::last_os_error().raw_os_error() == Some(errno) {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(libc::EINVAL))
    }
}

#[test]
#[ignore = "requires approved Kotlin worker and delegated capped cgroup v2"]
fn kotlin_kernel_boundary() {
    let exe = PathBuf::from(std::env::var_os("EON_TEST_KOTLIN").unwrap()).canonicalize().unwrap();
    let pin = std::env::var("EON_TEST_KOTLIN_SHA256").unwrap();
    assert_eq!(pin.len(), 64);
    let digest = std::array::from_fn(|i| u8::from_str_radix(&pin[i*2..i*2+2], 16).unwrap());
    let group = Path::new("/sys/fs/cgroup").join(
        fs::read_to_string("/proc/self/cgroup").unwrap().trim().strip_prefix("0::/").unwrap()
    );
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"));
    let io = WorkerIo::Kotlin { group: &group, workspace, digest: &digest,
        deadline: std::time::Instant::now() + std::time::Duration::from_secs(12) };
    let mut cmd = Command::new(&exe);
    let policy = ConfinementPolicy { read_only: vec![exe.clone()], deny_network: true };
    let mut broad = policy.clone();
    broad.read_only.push(exe.parent().unwrap().into());
    assert!(confine_command(&mut Command::new(&exe), &broad, io).is_err());
    confine_command(&mut cmd, &policy, io).unwrap();
    let source = std::ffi::CString::new(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
    // SAFETY: child-only negative probes after all production filters. No data is
    // written and no signal or connection is sent to any external target.
    unsafe {
        cmd.pre_exec(move || {
            for p in [source.as_c_str(), c"/etc/passwd", c"/proc/self/maps", c"kotlin-license/LICENSE.txt"] {
                denied(libc::open(p.as_ptr(), libc::O_RDONLY).into(), libc::EACCES)?;
            }
            denied(libc::open(c"worker.jar".as_ptr(), libc::O_WRONLY).into(), libc::EPERM)?;
            for domain in [libc::AF_INET, libc::AF_UNIX] {
                denied(libc::socket(domain, libc::SOCK_STREAM, 0).into(), libc::EPERM)?;
            }
            let mut pair = [-1; 2];
            denied(libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, pair.as_mut_ptr()).into(), libc::EPERM)?;
            denied(libc::syscall(libc::SYS_connect, -1, 0usize, 0usize), libc::EPERM)?;
            denied(libc::syscall(libc::SYS_clone3, 0usize, 0usize), libc::ENOSYS)?;
            let child = libc::fork();
            if child == 0 { libc::_exit(1); }
            denied(child.into(), libc::EPERM)?;
            let argv = [c"sh".as_ptr(), std::ptr::null()];
            denied(libc::execve(c"/bin/sh".as_ptr(), argv.as_ptr(), argv[1..].as_ptr()).into(), libc::EACCES)?;
            // No external PID, other resource, or non-null new-limit pointer.
            for (pid, res, new) in [(1, libc::RLIMIT_NOFILE, 0usize), (0, libc::RLIMIT_AS, 0),
                                    (0, libc::RLIMIT_NOFILE, 1)] {
                denied(libc::syscall(libc::SYS_prlimit64, pid, res, new, 0usize), libc::EPERM)?;
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().unwrap();
    let proc = Path::new("/proc").join(child.id().to_string());
    let filter_count = |p: &Path| {
        fs::read_to_string(p).ok()?.lines()
            .find_map(|l| l.strip_prefix("Seccomp_filters:"))?.trim().parse::<usize>().ok()
    };
    // Observe the clone3 denial, main allowlist and additional no-exec seal.
    let expected = filter_count(Path::new("/proc/self/status")).unwrap() + 3;
    let until = std::time::Instant::now() + std::time::Duration::from_secs(4);
    let status = proc.join("status");
    while filter_count(&status) != Some(expected) && std::time::Instant::now() < until {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let sealed = filter_count(&status);
    let image = fs::read_link(proc.join("exe"));
    let limits = fs::read_to_string(proc.join("limits"));
    let members = fs::read_to_string(group.join("cgroup.procs"));
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(sealed, Some(expected));
    assert_eq!(image.unwrap(), exe);
    assert!(members.unwrap().lines().any(|p| p == child.id().to_string()));
    let limits = limits.unwrap();
    for (prefix, cap) in [
        ("Max cpu time", 60), ("Max address space", 2_u64 << 30),
        ("Max open files", 64), ("Max file size", 0), ("Max core file size", 0),
    ] {
        let line = limits.lines().find_map(|s| s.strip_prefix(prefix)).unwrap();
        assert!(line.split_whitespace().take(2).all(|s| s.parse::<u64>().unwrap() <= cap));
    }
}
