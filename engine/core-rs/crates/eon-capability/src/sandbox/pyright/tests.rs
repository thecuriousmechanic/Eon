use crate::sandbox::{ConfinementPolicy, WorkerIo, confine_command};
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

#[test]
#[ignore = "same Pyright SEA, pinned digest and capped cgroup as the semantic gate"]
fn lsp_and_patcher_tests_pyright_kernel_boundary() {
    let exe = PathBuf::from(std::env::var_os("EON_TEST_PYRIGHT").unwrap())
        .canonicalize()
        .unwrap();
    let stubs = exe.with_file_name("typeshed-fallback").join("stdlib");
    let pin = std::env::var("EON_TEST_PYRIGHT_SHA256").unwrap();
    let digest: [u8; 32] =
        std::array::from_fn(|i| u8::from_str_radix(&pin[i * 2..i * 2 + 2], 16).unwrap());
    let group = Path::new("/sys/fs/cgroup").join(
        fs::read_to_string("/proc/self/cgroup")
            .unwrap()
            .trim()
            .strip_prefix("0::/")
            .unwrap(),
    );
    let names =
        std::env::temp_dir().join(format!("eon-denials-{:016x}", getrandom::u64().unwrap()));
    fs::DirBuilder::new().mode(0o700).create(&names).unwrap();
    let marker = names.join("module.py");
    fs::File::create_new(&marker).unwrap();
    let libs = std::env::var_os("EON_TEST_PYRIGHT_RUNTIME").unwrap();
    let mut read_only: Vec<_> = std::env::split_paths(&libs)
        .map(|p| p.canonicalize().unwrap())
        .collect();
    read_only.extend([
        exe.clone(),
        stubs.clone(),
        stubs.parent().unwrap().into(),
        names.clone(),
    ]);
    let policy = ConfinementPolicy {
        read_only,
        deny_network: true,
    };
    let worker = |digest| WorkerIo::Pyright {
        group: &group,
        names: &names,
        stubs: &stubs,
        digest,
    };
    let err = confine_command(&mut Command::new(&exe), &policy, worker(&[0; 32])).unwrap_err();
    assert!(err.to_string().contains("unapproved Pyright SEA digest"));
    let mut cmd = Command::new(&exe);
    cmd.arg("--stdio");
    let expected_image = fs::File::open(&exe).unwrap();
    confine_command(&mut cmd, &policy, worker(&digest)).unwrap();
    let marker_c = CString::new(marker.as_os_str().as_bytes()).unwrap();
    // SAFETY: only async-signal-safe negative probes in our own child, after
    // the production filters. No socket connection, process attachment or writes.
    unsafe {
        cmd.pre_exec(move || {
            let denied = |ret, errno| {
                if ret == -1 && io::Error::last_os_error().raw_os_error() == Some(errno) {
                    Ok(())
                } else {
                    Err(io::Error::from_raw_os_error(libc::EINVAL))
                }
            };
            for path in [marker_c.as_c_str(), c"/etc/passwd"] {
                denied(libc::open(path.as_ptr(), libc::O_RDONLY), libc::EACCES)?;
            }
            denied(libc::open(marker_c.as_ptr(), libc::O_WRONLY), libc::EPERM)?;
            denied(
                libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0),
                libc::EPERM,
            )?;
            denied(libc::fcntl(0, libc::F_DUPFD, 3), libc::EPERM)?;
            denied(
                libc::ioctl(0, libc::TIOCGWINSZ, std::ptr::null_mut::<libc::winsize>()),
                libc::EPERM,
            )?;
            denied(
                libc::syscall(libc::SYS_io_uring_setup, 1, 0usize) as i32,
                libc::EPERM,
            )?;
            denied(libc::kill(libc::getpid(), 0), libc::EPERM)?;
            let r = libc::fork();
            if r == 0 {
                libc::_exit(1);
            }
            denied(r, libc::EPERM)?;
            let argv = [c"true".as_ptr(), std::ptr::null()];
            denied(
                libc::execve(c"/usr/bin/true".as_ptr(), argv.as_ptr(), argv[1..].as_ptr()),
                libc::EACCES,
            )
        });
    }
    let mut child = cmd.spawn().unwrap();
    // A successful exec in the last negative probe would also make spawn
    // succeed. Observe the actual inode while the child is still ours.
    let live_image = fs::metadata(format!("/proc/{}/exe", child.id()));
    let enrolled = fs::read_to_string(group.join("cgroup.procs"));
    let killed = child.kill();
    let reaped = child.wait();
    fs::remove_file(marker).unwrap();
    fs::remove_dir(names).unwrap();
    killed.unwrap();
    reaped.unwrap();
    let live = live_image.unwrap();
    let pinned = expected_image.metadata().unwrap();
    assert_eq!((live.dev(), live.ino()), (pinned.dev(), pinned.ino()));
    assert!(
        enrolled
            .unwrap()
            .lines()
            .any(|id| id == child.id().to_string())
    );
}
