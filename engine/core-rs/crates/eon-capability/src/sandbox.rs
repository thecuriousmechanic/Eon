//! Local stdio workers: deny-by-default syscall admission, read-only Landlock
//! grants, no inherited host environment or non-stdio descriptors. This is a
//! narrow ABI for operator-selected binaries, not a VM for adversarial code.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::AsFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use eon_primitives::{EonError, EonResult};
use landlock::{
    ABI, Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreated,
    RulesetCreatedAttr, RulesetStatus,
};
use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, TargetArch};

#[cfg(all(
    target_os = "linux",
    target_arch = "x86_64",
    target_env = "gnu",
    target_pointer_width = "64"
))]
mod rust_analyzer;

#[cfg(all(
    target_os = "linux",
    target_arch = "x86_64",
    target_env = "gnu",
    target_pointer_width = "64"
))]
mod pyright;

#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu", target_pointer_width = "64"))]
mod kotlin;

#[cfg(not(all(
    target_os = "linux",
    target_arch = "x86_64",
    target_env = "gnu",
    target_pointer_width = "64"
)))]
mod rust_analyzer {
    use super::*;

    pub(crate) fn check_abi() -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "rust-analyzer requires x86-64 glibc Linux",
        ))
    }

    pub(super) fn prepare(_: &mut Command, _: &Path) -> io::Result<()> {
        check_abi()
    }

    pub(super) fn executable_grant(
        _: &Path,
        _: &Path,
        _: &PathFd,
        _: &std::fs::Metadata,
    ) -> io::Result<bool> {
        Err(io::ErrorKind::Unsupported.into())
    }

    #[cfg(all(
        target_pointer_width = "64",
        any(
            target_arch = "aarch64",
            target_arch = "riscv64",
            target_arch = "x86_64"
        )
    ))]
    pub(super) fn thread_filter(
        _: &mut BTreeMap<i64, Vec<seccompiler::SeccompRule>>,
        _: TargetArch,
    ) -> EonResult<BpfProgram> {
        Err(EonError::SecurityViolation(
            "unsupported rust-analyzer worker ABI".into(),
        ))
    }
}

pub(crate) use rust_analyzer::check_abi as check_rust_analyzer_abi;

/// Read/execute grants for a trusted worker. Socket access is not a supported
/// launch mode: explicitly requesting `deny_network: false` fails admission.
#[derive(Debug, Clone)]
pub struct ConfinementPolicy {
    pub read_only: Vec<PathBuf>,
    pub deny_network: bool,
}

impl Default for ConfinementPolicy {
    fn default() -> Self {
        Self {
            read_only: vec![],
            deny_network: true,
        }
    }
}

fn sandbox_err(context: &str, e: impl std::fmt::Display) -> EonError {
    EonError::OperationFailed {
        operation: format!("sandbox.{context}"),
        reason: e.to_string(),
    }
}

fn build_ruleset(
    policy: &ConfinementPolicy,
    program: &Path,
    io: WorkerIo<'_>,
) -> EonResult<RulesetCreated> {
    let abi = ABI::V3;
    let mut created = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .and_then(|r| r.create())
        .map_err(|e| sandbox_err("ruleset", e))?;
    if let WorkerIo::Kotlin { digest, workspace, deadline, .. } = io {
        #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu", target_pointer_width = "64"))]
        return kotlin::ruleset(created, program, &policy.read_only, workspace, digest, deadline);
        #[cfg(not(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu", target_pointer_width = "64")))]
        {
            let _ = (digest, workspace, deadline);
            return Err(sandbox_err("Kotlin", "unsupported worker ABI"));
        }
    }
    for path in &policy.read_only {
        let fd = PathFd::new(path)
            .map_err(|e| sandbox_err("path", format!("{}: {e}", path.display())))?;
        // A rule directly on a regular executable/library cannot grant ReadDir.
        // Inspect the pinned descriptor, not the possibly replaced pathname.
        let metadata = fd
            .as_fd()
            .try_clone_to_owned()
            .map(std::fs::File::from)
            .and_then(|f| f.metadata())
            .map_err(|e| sandbox_err("path metadata", e))?;
        let read = if metadata.is_dir() {
            AccessFs::from_read(abi)
        } else {
            AccessFs::ReadFile.into()
        };
        let access = if let WorkerIo::Pyright { names, stubs, .. } = io
            && (path == names || path == stubs || Some(path.as_path()) == stubs.parent())
        {
            if !metadata.is_dir() {
                return Err(sandbox_err("pyright", "expected directory"));
            }
            if path != stubs {
                AccessFs::ReadDir.into()
            } else {
                AccessFs::ReadDir | AccessFs::ReadFile
            }
        } else if io.thread_group().is_some() {
            if let Some((name, digest)) = io.node_image()
                && path == program
            {
                #[cfg(all(
                    target_os = "linux",
                    target_arch = "x86_64",
                    target_env = "gnu",
                    target_pointer_width = "64"
                ))]
                pyright::verify(&fd, &metadata, name, digest)
                    .map_err(|e| sandbox_err("LSP SEA", e))?;
                #[cfg(not(all(
                    target_os = "linux",
                    target_arch = "x86_64",
                    target_env = "gnu",
                    target_pointer_width = "64"
                )))]
                {
                    let _ = (name, digest);
                    return Err(sandbox_err("LSP SEA", "unsupported ABI"));
                }
            }
            if rust_analyzer::executable_grant(path, program, &fd, &metadata)
                .map_err(|e| sandbox_err("rust-analyzer grant", e))?
            {
                read | AccessFs::Execute
            } else {
                read
            }
        } else {
            read | AccessFs::Execute
        };
        created = created
            .add_rule(PathBeneath::new(fd, access))
            .map_err(|e| sandbox_err("rule", e))?;
    }
    Ok(created)
}

#[cfg(all(
    target_pointer_width = "64",
    any(
        target_arch = "aarch64",
        target_arch = "riscv64",
        target_arch = "x86_64"
    )
))]
fn worker_filters(io: WorkerIo<'_>) -> EonResult<Vec<BpfProgram>> {
    use libc::*;
    // Unknown/future syscalls are refused. No socket, descriptor acquisition
    // from other processes, process creation/control, ioctl, AIO or io_uring.
    let allowed = [
        SYS_execve,
        SYS_exit,
        SYS_exit_group,
        SYS_read,
        SYS_readv,
        SYS_pread64,
        SYS_write,
        SYS_writev,
        SYS_close,
        SYS_openat,
        // Do not guess missing bionic syscall numbers; leave them denied.
        #[cfg(not(all(target_os = "android", not(target_arch = "x86_64"))))]
        SYS_fstat,
        #[cfg(not(all(target_os = "android", not(target_arch = "x86_64"))))]
        SYS_newfstatat,
        SYS_statx,
        SYS_faccessat,
        SYS_readlinkat,
        SYS_getdents64,
        SYS_lseek,
        SYS_getcwd,
        SYS_brk,
        SYS_mmap,
        SYS_mremap,
        SYS_mprotect,
        SYS_munmap,
        SYS_madvise,
        SYS_rt_sigaction,
        SYS_rt_sigprocmask,
        SYS_rt_sigreturn,
        SYS_sigaltstack,
        SYS_set_tid_address,
        SYS_set_robust_list,
        SYS_futex,
        #[cfg(not(target_os = "android"))]
        SYS_rseq,
        SYS_getpid,
        SYS_gettid,
        SYS_getuid,
        SYS_geteuid,
        SYS_getgid,
        SYS_getegid,
        SYS_clock_gettime,
        SYS_clock_nanosleep,
        SYS_nanosleep,
        SYS_getrandom,
        SYS_sched_getaffinity,
        SYS_ppoll,
        SYS_pselect6,
        #[cfg(target_arch = "x86_64")]
        SYS_arch_prctl,
        #[cfg(target_arch = "x86_64")]
        SYS_access,
        #[cfg(target_arch = "x86_64")]
        SYS_readlink,
        #[cfg(target_arch = "x86_64")]
        SYS_stat,
        #[cfg(target_arch = "x86_64")]
        SYS_lstat,
        #[cfg(target_arch = "x86_64")]
        SYS_poll,
    ];
    let mut rules = allowed
        .into_iter()
        .map(|nr| (nr, vec![]))
        .collect::<BTreeMap<_, _>>();
    // Runtimes query FD_CLOEXEC on their own stdio. Do not grant the rest of fcntl.
    use seccompiler::{SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompRule};
    let query = SeccompCondition::new(1, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, F_GETFD as u64)
        .and_then(|cond| SeccompRule::new(vec![cond]))
        .map_err(|e| sandbox_err("seccomp", e))?;
    rules.insert(SYS_fcntl, vec![query]);
    let arch =
        TargetArch::try_from(std::env::consts::ARCH).map_err(|e| sandbox_err("architecture", e))?;
    let mut filters = Vec::new();
    if io.thread_group().is_some() {
        filters.push(rust_analyzer::thread_filter(&mut rules, arch)?);
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu", target_pointer_width = "64"))]
    if matches!(io, WorkerIo::Kotlin { .. }) {
        kotlin::jvm_filter(&mut rules)?;
    }
    if io.node_image().is_some() {
        #[cfg(all(
            target_os = "linux",
            target_arch = "x86_64",
            target_env = "gnu",
            target_pointer_width = "64"
        ))]
        pyright::event_loop(&mut rules)?;
        #[cfg(not(all(
            target_os = "linux",
            target_arch = "x86_64",
            target_env = "gnu",
            target_pointer_width = "64"
        )))]
        return Err(sandbox_err("Node LSP", "unsupported ABI"));
    }
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Errno(libc::EPERM as u32),
        SeccompAction::Allow,
        arch,
    )
    .map_err(|e| sandbox_err("seccomp", e))?;
    filters.push(
        filter
            .try_into()
            .map_err(|e: seccompiler::BackendError| sandbox_err("seccomp", e))?,
    );
    Ok(filters)
}

#[cfg(not(all(
    target_pointer_width = "64",
    any(
        target_arch = "aarch64",
        target_arch = "riscv64",
        target_arch = "x86_64"
    )
)))]
fn worker_filters(_: WorkerIo<'_>) -> EonResult<Vec<BpfProgram>> {
    Err(EonError::SecurityViolation(
        "unsupported native worker ABI".into(),
    ))
}

#[derive(Clone, Copy)]
pub(crate) enum WorkerIo<'a> {
    ClosedInput,
    StdioProtocol,
    RustAnalyzer(&'a Path),
    Pyright {
        group: &'a Path,
        names: &'a Path,
        stubs: &'a Path,
        digest: &'a [u8; 32],
    },
    Vtsls {
        group: &'a Path,
        digest: &'a [u8; 32],
    },
    Kotlin {
        group: &'a Path,
        digest: &'a [u8; 32],
        workspace: &'a Path,
        deadline: std::time::Instant,
    },
}

impl<'a> WorkerIo<'a> {
    fn thread_group(self) -> Option<&'a Path> {
        match self {
            Self::RustAnalyzer(group) | Self::Pyright { group, .. } | Self::Vtsls { group, .. } | Self::Kotlin { group, .. } => {
                Some(group)
            }
            _ => None,
        }
    }

    fn node_image(self) -> Option<(&'static str, &'a [u8; 32])> {
        match self {
            Self::Pyright { digest, .. } => Some(("Pyright", digest)),
            Self::Vtsls { digest, .. } => Some(("vtsls", digest)),
            _ => None,
        }
    }
}

/// Runs `program` with `args` under `policy` and returns its output. Fails if confinement cannot be enforced.
pub fn run_confined(program: &str, args: &[&str], policy: &ConfinementPolicy) -> EonResult<Output> {
    let mut command = Command::new(program);
    command.args(args);
    confine_command(&mut command, policy, WorkerIo::ClosedInput)?;
    command.output().map_err(|e| sandbox_err("spawn", e))
}

pub(crate) fn confine_command(
    command: &mut Command,
    policy: &ConfinementPolicy,
    io: WorkerIo<'_>,
) -> EonResult<()> {
    if !Path::new(command.get_program()).is_absolute() || !policy.deny_network {
        return Err(EonError::SecurityViolation(
            "native worker requires an absolute program and denied network".into(),
        ));
    }
    command.env_clear().current_dir("/").stdout(Stdio::piped());
    match io {
        WorkerIo::ClosedInput => command.stdin(Stdio::null()).stderr(Stdio::piped()),
        WorkerIo::StdioProtocol
        | WorkerIo::RustAnalyzer(_)
        | WorkerIo::Pyright { .. }
        | WorkerIo::Kotlin { .. }
        | WorkerIo::Vtsls { .. } => {
            command.stdin(Stdio::piped()).stderr(Stdio::null())
        }
    };
    if let Some(group) = io.thread_group() {
        rust_analyzer::prepare(command, group)
            .map_err(|e| sandbox_err("threaded LSP limits", e))?;
    }
    if let WorkerIo::Kotlin { .. } = io {
        let install = Path::new(command.get_program()).parent()
            .ok_or_else(|| sandbox_err("Kotlin", "missing installation directory"))?.to_owned();
        command.current_dir(install).env("MALLOC_ARENA_MAX", "1");
    }
    if io.node_image().is_some() {
        // This worker performs no TLS. Do not read the host OpenSSL config.
        let parent = Path::new(command.get_program())
            .parent()
            .ok_or_else(|| sandbox_err("Node LSP", "missing installation directory"))?
            .to_owned();
        command
            .current_dir(parent)
            .env("OPENSSL_CONF", "")
            .env("TMPDIR", "/eon-no-scratch");
    }
    let mut ruleset = Some(build_ruleset(policy, command.get_program().as_ref(), io)?);
    let filters = worker_filters(io)?;
    // SAFETY: the closure runs in the forked child before exec. It takes prebuilt objects, calls `restrict_self`
    // (Landlock and no_new_privs) and then `apply_filter` (seccomp), without formatting errors or taking a lock.
    unsafe {
        command.pre_exec(move || {
            let denied = || io::Error::from_raw_os_error(libc::EPERM);
            // Preserve the spawn error pipe until exec. No ambient descriptor
            // survives a successful exec; an unsupported close_range aborts it.
            // Linux UAPI <linux/close_range.h>; bionic libc does not export the flag.
            #[cfg(target_os = "android")]
            const CLOEXEC: u32 = 1 << 2;
            #[cfg(not(target_os = "android"))]
            const CLOEXEC: u32 = libc::CLOSE_RANGE_CLOEXEC;
            if libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, CLOEXEC) < 0 {
                return Err(io::Error::last_os_error());
            }
            let created = ruleset.take().ok_or_else(denied)?;
            match created.restrict_self() {
                Ok(status)
                    if status.ruleset == RulesetStatus::FullyEnforced && status.no_new_privs => {}
                _ => return Err(denied()),
            }
            for filter in &filters {
                seccompiler::apply_filter(filter).map_err(|_| denied())?;
            }
            Ok(())
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "eon-sandbox-{name}-{:016x}",
            getrandom::u64().unwrap()
        ));
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    #[test]
    fn confined_process_reads_allowed_file_and_is_denied_outside() {
        let allowed = fixture_dir("allowed");
        let secret = fixture_dir("secret");
        std::fs::write(allowed.join("ok.txt"), "allowed-content").unwrap();
        std::fs::write(secret.join("key.txt"), "TOP-SECRET").unwrap();

        let policy = ConfinementPolicy {
            read_only: vec![
                allowed.clone(),
                PathBuf::from("/usr"),
                PathBuf::from("/lib"),
                PathBuf::from("/lib64"),
                PathBuf::from("/bin"),
            ],
            deny_network: true,
        };
        let ok = run_confined(
            "/bin/cat",
            &[allowed.join("ok.txt").to_str().unwrap()],
            &policy,
        )
        .unwrap();
        assert!(
            ok.status.success(),
            "allowed read failed: {}",
            String::from_utf8_lossy(&ok.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&ok.stdout), "allowed-content");

        let denied = run_confined(
            "/bin/cat",
            &[secret.join("key.txt").to_str().unwrap()],
            &policy,
        )
        .unwrap();
        assert!(
            !denied.status.success(),
            "read outside the policy succeeded"
        );
        assert!(denied.stdout.is_empty());

        let sentinel = allowed.join("ok.txt");
        let write = run_confined(
            "/usr/bin/truncate",
            &["-s", "0", sentinel.to_str().unwrap()],
            &policy,
        )
        .unwrap();
        assert!(!write.status.success(), "read-only rule allowed truncation");
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"allowed-content");

        std::fs::remove_dir_all(allowed).unwrap();
        std::fs::remove_dir_all(secret).unwrap();
    }

    fn socket_probe() -> (&'static str, Vec<&'static str>) {
        const PROBE: &str = r#"
import socket, errno
try:
    socket.socket()
except PermissionError as e:
    if e.errno != errno.EPERM:
        raise
    print('socket-denied')
    raise SystemExit(1)
print('socket-ok')
"#;
        ("/usr/bin/python3", vec!["-I", "-c", PROBE])
    }

    #[test]
    fn native_worker_refuses_a_network_enabled_policy_before_spawn() {
        let (program, args) = socket_probe();
        let policy = ConfinementPolicy {
            read_only: vec![
                PathBuf::from("/usr"),
                PathBuf::from("/lib"),
                PathBuf::from("/lib64"),
                PathBuf::from("/etc"),
            ],
            deny_network: false,
        };
        assert!(matches!(
            run_confined(program, &args, &policy),
            Err(EonError::SecurityViolation(_))
        ));
    }

    #[test]
    fn socket_creation_is_refused_when_network_is_denied() {
        let (program, args) = socket_probe();
        let policy = ConfinementPolicy {
            read_only: vec![
                PathBuf::from("/usr"),
                PathBuf::from("/lib"),
                PathBuf::from("/lib64"),
                PathBuf::from("/etc"),
            ],
            deny_network: true,
        };
        let out = run_confined(program, &args, &policy).unwrap();
        assert!(
            !out.status.success(),
            "socket creation succeeded under deny_network"
        );
        assert_eq!(
            out.stdout,
            b"socket-denied\n",
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn no_host_environment_input_or_nonstdio_descriptor_reaches_exec() {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        let host = std::fs::File::open("/etc/os-release").unwrap();
        // Reserve a high descriptor without changing inheritance in this process.
        // SAFETY: host is live, and a successful F_DUPFD_CLOEXEC returns a new fd.
        let fd = unsafe { libc::fcntl(host.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 64) };
        assert!(fd >= 64);
        let inherited = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut cmd = Command::new("/usr/bin/python3");
        cmd.env("EON_ENV_FIXTURE", "private").stdin(host).args([
            "-I",
            "-S",
            "-c",
            r#"
import os, sys, errno
assert 'EON_ENV_FIXTURE' not in os.environ
assert os.read(0, 1) == b''
try: os.fstat(int(sys.argv[1]))
except OSError as e: assert e.errno == errno.EBADF
else: raise AssertionError('inherited descriptor')
print('clean')
"#,
            &fd.to_string(),
        ]);
        // SAFETY: only changes the fixture descriptor flags in the forked child,
        // before installing the normal worker policy.
        unsafe {
            cmd.pre_exec(move || {
                if libc::fcntl(inherited.as_raw_fd(), libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let policy = ConfinementPolicy {
            read_only: vec!["/usr".into(), "/lib".into(), "/lib64".into()],
            ..Default::default()
        };
        confine_command(&mut cmd, &policy, WorkerIo::ClosedInput).unwrap();
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(out.stdout, b"clean\n");
    }

    #[test]
    fn default_deny_includes_async_io_without_submitting_any_io() {
        let mut cmd = Command::new("/bin/true");
        let policy = ConfinementPolicy {
            read_only: vec!["/usr".into(), "/lib".into(), "/lib64".into(), "/bin".into()],
            ..Default::default()
        };
        confine_command(&mut cmd, &policy, WorkerIo::ClosedInput).unwrap();
        // SAFETY: inert policy probes in the already-filtered child. No ring,
        // context, buffers or requests are created. An ordinary bad-fd/argument
        // response is not evidence that the worker filter denied the syscall.
        unsafe {
            cmd.pre_exec(|| {
                if libc::syscall(
                    libc::SYS_io_uring_enter,
                    -1i32,
                    0u32,
                    0u32,
                    0u32,
                    0usize,
                    0usize,
                ) != -1
                    || io::Error::last_os_error().raw_os_error() != Some(libc::EPERM)
                {
                    return Err(io::Error::from_raw_os_error(libc::EINVAL));
                }
                if libc::syscall(libc::SYS_io_submit, 0usize, 0usize, 0usize) != -1
                    || io::Error::last_os_error().raw_os_error() != Some(libc::EPERM)
                {
                    return Err(io::Error::from_raw_os_error(libc::EINVAL));
                }
                Ok(())
            });
        }
        assert!(cmd.output().unwrap().status.success());
    }
}
