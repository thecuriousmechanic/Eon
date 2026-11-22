//! Additional libuv operations for the fixed Node LSP images. The inherited
//! pthread/cgroup/rlimit and read-only openat rules remain in effect.
use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsFd, AsRawFd};

use libc::*;
use seccompiler::{SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompRule};

use super::{EonResult, sandbox_err};

#[cfg(test)]
mod tests;
#[cfg(test)]
mod vtsls_tests;

pub(super) fn verify(
    fd: &landlock::PathFd,
    meta: &std::fs::Metadata,
    name: &str,
    expected: &[u8; 32],
) -> io::Result<()> {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::MetadataExt;
    if !meta.is_file() || meta.mode() & 0o6222 != 0 || meta.len() > 256 * 1024 * 1024 {
        return Err(io::Error::other(format!("expected immutable {name} SEA")));
    }
    let mut image = std::fs::File::open(format!("/proc/self/fd/{}", fd.as_fd().as_raw_fd()))?;
    let mut hash = Sha256::new();
    io::copy(&mut image, &mut hash)?;
    if hash.finalize().as_slice() != expected {
        return Err(io::Error::other(format!("unapproved {name} SEA digest")));
    }
    Ok(())
}

pub(super) fn event_loop(rules: &mut BTreeMap<i64, Vec<SeccompRule>>) -> EonResult<()> {
    use SeccompCmpArgLen::{Dword, Qword};
    use SeccompCmpOp::Eq;
    let rule = |args: &[(u8, SeccompCmpArgLen, u64)]| {
        args.iter()
            .map(|(i, width, value)| SeccompCondition::new(*i, width.clone(), Eq, *value))
            .collect::<Result<Vec<_>, _>>()
            .and_then(SeccompRule::new)
            .map_err(|e| sandbox_err("pyright seccomp", e))
    };
    rules.insert(
        SYS_epoll_create1,
        vec![rule(&[(0, Dword, EPOLL_CLOEXEC as u64)])?],
    );
    // Only the worker's own descriptors survive exec. No sockets, io_uring,
    // pidfds, dup/fd passing, watchers or external signal delivery.
    // Node's built-in os module also requires the read-only uname query.
    for sys in [SYS_epoll_ctl, SYS_epoll_pwait, SYS_uname] {
        rules.insert(sys, vec![]);
    }
    rules.insert(
        SYS_pipe2,
        [O_CLOEXEC, O_CLOEXEC | O_NONBLOCK]
            .iter()
            .map(|&f| rule(&[(1, Dword, f as u64)]))
            .collect::<EonResult<_>>()?,
    );
    rules.insert(
        SYS_eventfd2,
        vec![rule(&[
            (0, Dword, 0),
            (1, Dword, (EFD_CLOEXEC | EFD_NONBLOCK) as u64),
        ])?],
    );
    // libuv posts an eight-byte eventfd counter or a one-byte pipe wakeup. No writable
    // files can be opened; the only inherited writable FDs are stdout/stderr.
    for size in [1, 8] {
        rules
            .get_mut(&SYS_write)
            .expect("thread profile")
            .push(rule(&[(2, Qword, size)])?);
    }
    let fcntl = rules.get_mut(&SYS_fcntl).expect("base profile");
    fcntl.push(rule(&[(1, Dword, F_GETFL as u64)])?);
    fcntl.push(rule(&[
        (1, Dword, F_SETFD as u64),
        (2, Dword, FD_CLOEXEC as u64),
    ])?);
    for (fd, flags) in [(0, O_RDONLY), (1, O_WRONLY), (2, O_WRONLY)] {
        fcntl.push(rule(&[
            (0, Dword, fd),
            (1, Dword, F_SETFL as u64),
            (2, Dword, flags as u64),
        ])?);
    }
    rules.insert(
        SYS_ioctl,
        (0..=2)
            .map(|fd| rule(&[(0, Dword, fd), (1, Qword, FIONBIO)]))
            .collect::<EonResult<_>>()?,
    );
    Ok(())
}
