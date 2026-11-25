//! Fixed one-shot allocator. No interpreter, supplied argv, input protocol,
//! CommandBuilder, or worker spawn. Only the daemon launches the confined filter.
#[cfg(not(all(target_os = "linux", target_env = "gnu", target_arch = "x86_64")))]
compile_error!("portable-pty-allocator requires x86_64 glibc Linux");

mod grant;
#[path = "../native/host.rs"]
mod host;

use std::fs::{self, File};
use std::io::{self, IoSlice};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::MetadataExt;
use std::process::ExitCode;

use grant::{FRAME, Grant, denied};
use landlock::{
    ABI, Access, AccessFs, PathBeneath, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus,
};
use portable_pty::{PtySize, native_pty_system};
use rustix::net::{
    SendAncillaryBuffer, SendAncillaryMessage, SendFlags, SocketType, sendmsg, sockopt,
};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};

fn seal(ptmx: File) -> io::Result<()> {
    // Linux checks Landlock on the dentry opened by TIOCGPTPEER, too. Grant
    // only this devpts instance (not /dev, procfs or a host library tree).
    let pts = File::open("/dev/pts")?;
    if pts.metadata()?.dev() != ptmx.metadata()?.dev() {
        return Err(denied());
    }
    let abi = ABI::V3;
    let status = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .and_then(|r| r.create())
        // The bind-mounted /dev/ptmx is a second path to this exact inode.
        .and_then(|r| {
            r.add_rule(PathBeneath::new(
                ptmx,
                AccessFs::ReadFile | AccessFs::WriteFile,
            ))
        })
        .and_then(|r| {
            r.add_rule(PathBeneath::new(
                pts,
                AccessFs::ReadFile | AccessFs::WriteFile,
            ))
        })
        .and_then(|r| r.restrict_self())
        .map_err(io::Error::other)?;
    if !status.no_new_privs || status.ruleset != RulesetStatus::FullyEnforced {
        return Err(denied());
    }
    // In particular no clone/fork/exec, new sockets, ptrace, namespace or
    // mount changes, byte reads or writes. glibc opens ptmx with O_RDWR;
    // its pathname-based slave fallback (O_RDWR|O_NOCTTY) is *not* allowed.
    use libc::*;
    let mut rules: std::collections::BTreeMap<_, _> = [
        SYS_brk,
        SYS_close,
        SYS_exit,
        SYS_exit_group,
        SYS_fcntl,
        SYS_fstat,
        SYS_futex,
        SYS_madvise,
        SYS_mmap,
        SYS_mremap,
        SYS_munmap,
        SYS_newfstatat,
        SYS_readlink,
        SYS_readlinkat,
        SYS_rt_sigreturn,
        SYS_sigaltstack,
        SYS_statx,
    ]
    .into_iter()
    .map(|s| (s, vec![]))
    .collect();
    let eq = |arg, value| {
        SeccompCondition::new(arg, SeccompCmpArgLen::Qword, SeccompCmpOp::Eq, value)
            .and_then(|c| SeccompRule::new(vec![c]))
            .map_err(io::Error::other)
    };
    rules.insert(SYS_openat, vec![eq(2, O_RDWR as u64)?]);
    rules.insert(SYS_sendmsg, vec![eq(0, STDOUT_FILENO as u64)?]);
    rules.insert(
        SYS_ioctl,
        [TIOCGPTN, TIOCSPTLCK, TIOCGPTPEER, TIOCSWINSZ, TCGETS]
            .into_iter()
            .map(|request| eq(1, request))
            .collect::<io::Result<_>>()?,
    );
    let filter: BpfProgram = SeccompFilter::new(
        rules,
        SeccompAction::Errno(EPERM as u32),
        SeccompAction::Allow,
        TargetArch::x86_64,
    )
    .and_then(TryInto::try_into)
    .map_err(io::Error::other)?;
    seccompiler::apply_filter(&filter).map_err(io::Error::other)
}

fn allocate() -> io::Result<()> {
    let out = io::stdout();
    if std::env::args_os().count() != 1 || std::env::vars_os().next().is_some()
        || sockopt::socket_type(&out)? != SocketType::SEQPACKET
        // Three stdio descriptors plus read_dir's own descriptor.
        || fs::read_dir("/proc/thread-self/fd")?.count() != 4
        || fs::read_dir("/proc/self/task")?.count() != 1
    {
        return Err(denied());
    }
    let Grant { image, ptmx } = Grant::pin()?;
    let expected = image.metadata()?;
    let running = fs::metadata("/proc/self/exe")?;
    if (expected.dev(), expected.ino()) != (running.dev(), running.ino()) {
        return Err(denied());
    }
    drop(image);
    seal(ptmx)?;
    let pair = native_pty_system()
        .openpty(PtySize::default())
        .map_err(io::Error::other)?;
    let fd = pair.master.as_raw_fd().ok_or_else(denied)?;
    // SAFETY: the upstream pair owns this live master through sendmsg.
    let master = unsafe {
        if libc::ioctl(fd, libc::TIOCSPTLCK, &1_i32) < 0 {
            return Err(io::Error::last_os_error());
        }
        BorrowedFd::borrow_raw(fd)
    };
    let fds = [master];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    if !control.push(SendAncillaryMessage::ScmRights(&fds))
        || sendmsg(
            out.as_fd(),
            &[IoSlice::new(FRAME)],
            &mut control,
            SendFlags::NOSIGNAL,
        )? != FRAME.len()
    {
        return Err(denied());
    }
    Ok(())
}

fn main() -> ExitCode {
    match allocate() {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}
