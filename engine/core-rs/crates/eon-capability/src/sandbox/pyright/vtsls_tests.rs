use crate::sandbox::{ConfinementPolicy, WorkerIo, confine_command};
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

#[test]
#[ignore = "requires approved fixed vtsls image and capped cgroup v2"]
fn vtsls_kernel_boundary() {
    let exe = PathBuf::from(std::env::var_os("EON_TEST_VTSLS").unwrap())
        .canonicalize()
        .unwrap();
    let pin = std::env::var("EON_TEST_VTSLS_SHA256").unwrap();
    assert_eq!(pin.len(), 64);
    let digest = std::array::from_fn(|i| u8::from_str_radix(&pin[i * 2..i * 2 + 2], 16).unwrap());
    let group = Path::new("/sys/fs/cgroup").join(
        fs::read_to_string("/proc/self/cgroup")
            .unwrap()
            .trim()
            .strip_prefix("0::/")
            .unwrap(),
    );
    let worker = |digest| WorkerIo::Vtsls {
        group: &group,
        digest,
    };
    let libs = std::env::var_os("EON_TEST_VTSLS_RUNTIME").unwrap();
    let mut policy = ConfinementPolicy {
        read_only: std::env::split_paths(&libs)
            .map(|p| p.canonicalize().unwrap())
            .collect(),
        deny_network: true,
    };
    policy.read_only.push(exe.clone());
    let configure = |policy: &ConfinementPolicy, digest| {
        let mut cmd = Command::new(&exe);
        cmd.arg("--stdio");
        confine_command(&mut cmd, policy, worker(digest)).map(|()| cmd)
    };
    assert!(
        configure(&policy, &[0; 32])
            .unwrap_err()
            .to_string()
            .contains("unapproved vtsls SEA digest")
    );
    let mut broad = policy.clone();
    broad.read_only.push(exe.parent().unwrap().into());
    assert!(configure(&broad, &digest).is_err());
    let mut cmd = configure(&policy, &digest).unwrap();
    let expected = fs::metadata(&exe).unwrap();
    let source =
        std::ffi::CString::new(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
    // SAFETY: child-only negative probes after the unmodified production
    // filters. No write, connection or signal is sent to any external target.
    unsafe {
        cmd.pre_exec(move || {
            let denied = |r: i64, errno| {
                if r == -1 && io::Error::last_os_error().raw_os_error() == Some(errno) {
                    Ok(())
                } else {
                    Err(io::Error::from_raw_os_error(libc::EINVAL))
                }
            };
            denied(
                libc::open(source.as_ptr(), libc::O_RDONLY).into(),
                libc::EACCES,
            )?;
            denied(
                libc::open(source.as_ptr(), libc::O_WRONLY).into(),
                libc::EPERM,
            )?;
            denied(
                libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0).into(),
                libc::EPERM,
            )?;
            denied(
                libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0).into(),
                libc::EPERM,
            )?;
            denied(
                libc::syscall(libc::SYS_clone3, 0usize, 0usize),
                libc::ENOSYS,
            )?;
            let child = libc::fork();
            if child == 0 {
                libc::_exit(1);
            }
            denied(child.into(), libc::EPERM)?;
            let argv = [c"true".as_ptr(), std::ptr::null()];
            denied(
                libc::execve(c"/usr/bin/true".as_ptr(), argv.as_ptr(), argv[1..].as_ptr()).into(),
                libc::EACCES,
            )
        });
    }
    let mut child = cmd.spawn().unwrap();
    let live = fs::metadata(format!("/proc/{}/exe", child.id()));
    let members = fs::read_to_string(group.join("cgroup.procs"));
    child.kill().unwrap();
    child.wait().unwrap();
    let live = live.unwrap();
    assert_eq!((live.dev(), live.ino()), (expected.dev(), expected.ino()));
    assert!(
        members
            .unwrap()
            .lines()
            .any(|id| id == child.id().to_string())
    );
}
