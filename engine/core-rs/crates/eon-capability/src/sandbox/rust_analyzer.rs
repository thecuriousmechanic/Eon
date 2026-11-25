//! Additional confinement for the native x86-64 glibc rust-analyzer only.
use std::collections::BTreeMap;
use std::fs::{File, Metadata};
use std::io::{self, Read};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use landlock::PathFd;
use rustix::fs::{self, Mode, OFlags};
use rustix::process::{self, Resource, Rlimit};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};

use super::{EonResult, sandbox_err};

const MAX_TASKS: u32 = 24;
const LOADER: &str = "/lib64/ld-linux-x86-64.so.2";

pub(crate) fn check_abi() -> io::Result<()> {
    Ok(())
}

pub(super) fn executable_grant(
    path: &Path,
    program: &Path,
    fd: &PathFd,
    meta: &Metadata,
) -> io::Result<bool> {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let loader = Path::new(LOADER).canonicalize()?;
    let server = path == program;
    let library = name
        .strip_prefix("lib")
        .and_then(|s| s.split_once(".so"))
        .is_some_and(|(stem, suffix)| {
            !stem.is_empty() && (suffix.is_empty() || suffix.starts_with('.'))
        });
    if !meta.is_file()
        || meta.mode() & 0o6022 != 0
        || !(meta.uid() == 0 || meta.uid() == process::geteuid().as_raw())
        || !(server || path == loader || library)
    {
        return Err(io::Error::other(
            "expected individual trusted native server/loader/library",
        ));
    }
    // Validate the same pinned inode the Landlock rule will use. No directory,
    // script, project source, config, device, shell or interpreter tree.
    let mut ident = [0; 20];
    File::open(format!("/proc/self/fd/{}", fd.as_fd().as_raw_fd()))?
        .read_exact_at(&mut ident, 0)?;
    if &ident[..7] != b"\x7fELF\x02\x01\x01"
        || ident[18..20] != [62, 0]
        || !(ident[16..18] == [3, 0] || server && ident[16..18] == [2, 0])
    {
        return Err(io::Error::other("expected x86-64 ELF image"));
    }
    Ok(server || path == loader)
}

fn limit(resource: Resource, cap: u64) -> io::Result<()> {
    let value = Some(process::getrlimit(resource).current.unwrap_or(cap).min(cap));
    Ok(process::setrlimit(
        resource,
        Rlimit {
            current: value,
            maximum: value,
        },
    )?)
}

pub(super) fn prepare(cmd: &mut Command, group: &Path) -> io::Result<()> {
    if !group.is_absolute() {
        return Err(io::Error::other("expected absolute task cgroup"));
    }
    let dir = fs::openat2(
        fs::CWD,
        group,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        fs::ResolveFlags::NO_SYMLINKS,
    )?;
    if fs::fstatfs(&dir)?.f_type != libc::CGROUP2_SUPER_MAGIC {
        return Err(io::Error::other("not an operator-provisioned cgroup v2"));
    }
    let open = |name, flags| {
        fs::openat2(
            &dir,
            name,
            flags | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
            fs::ResolveFlags::BENEATH | fs::ResolveFlags::NO_SYMLINKS | fs::ResolveFlags::NO_XDEV,
        )
    };
    let mut ceiling = String::new();
    File::from(open("pids.max", OFlags::RDONLY)?)
        .take(32)
        .read_to_string(&mut ceiling)?;
    if !matches!(ceiling.trim().parse::<u32>(), Ok(1..=MAX_TASKS)) {
        return Err(io::Error::other("rust-analyzer requires pids.max <= 24"));
    }
    let membership = open("cgroup.procs", OFlags::WRONLY)?;
    cmd.env("RAYON_NUM_THREADS", "1");
    // SAFETY: child-only syscalls before confinement; join the validated group
    // before allowing even one pthread. The CLOEXEC control fd is never an LSP grant.
    unsafe {
        cmd.pre_exec(move || {
            if rustix::io::write(&membership, b"0")? != 1 {
                return Err(io::Error::from_raw_os_error(libc::EIO));
            }
            limit(Resource::As, 2 * 1024 * 1024 * 1024)?;
            limit(Resource::Cpu, 60)?;
            limit(Resource::Nofile, 64)?;
            limit(Resource::Fsize, 0)?;
            limit(Resource::Core, 0)
        });
    }
    Ok(())
}

pub(super) fn thread_filter(
    rules: &mut BTreeMap<i64, Vec<SeccompRule>>,
    arch: TargetArch,
) -> EonResult<BpfProgram> {
    use SeccompCmpArgLen::{Dword, Qword};
    use SeccompCmpOp::{Eq, MaskedEq};
    use libc::*;
    let rule = |arg, width, cmp, value| {
        SeccompCondition::new(arg, width, cmp, value)
            .and_then(|cond| SeccompRule::new(vec![cond]))
            .map_err(|e| sandbox_err("rust-analyzer seccomp", e))
    };
    let pthread = CLONE_VM
        | CLONE_FS
        | CLONE_FILES
        | CLONE_SIGHAND
        | CLONE_THREAD
        | CLONE_SYSVSEM
        | CLONE_SETTLS
        | CLONE_PARENT_SETTID
        | CLONE_CHILD_CLEARTID;
    rules.insert(SYS_clone, vec![rule(0, Qword, Eq, pthread as u64)?]);
    let write_flags = O_ACCMODE | O_CREAT | O_TRUNC | O_APPEND | (O_TMPFILE & !O_DIRECTORY);
    rules.insert(
        SYS_openat,
        vec![rule(2, Dword, MaskedEq(write_flags as u64), 0)?],
    );
    for sys in [SYS_write, SYS_writev] {
        rules.insert(sys, vec![rule(0, Dword, Eq, 1)?, rule(0, Dword, Eq, 2)?]);
    }
    // Always install this unconditional denial *before* the main allowlist.
    // Only clone3 reports ENOSYS, selecting glibc's inspectable clone fallback.
    let special = SeccompFilter::new(
        BTreeMap::from([(SYS_clone3, vec![])]),
        SeccompAction::Allow,
        SeccompAction::Errno(ENOSYS as u32),
        arch,
    )
    .map_err(|e| sandbox_err("rust-analyzer clone3", e))?
    .try_into()
    .map_err(|e: seccompiler::BackendError| sandbox_err("rust-analyzer clone3", e))?;
    rules.insert(SYS_clone3, vec![]);
    Ok(special)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{ConfinementPolicy, WorkerIo, confine_command};
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;

    #[test]
    fn lsp_and_patcher_tests_reject_unbounded_or_ambient_rust_grants() {
        let mut cmd = Command::new("/usr/bin/true");
        assert!(prepare(&mut cmd, Path::new("relative")).is_err());
        assert!(prepare(&mut cmd, Path::new(env!("CARGO_MANIFEST_DIR"))).is_err());
        for path in ["/usr/lib", "/usr/bin/sh", "/etc/passwd"] {
            let path = Path::new(path);
            let fd = PathFd::new(path).unwrap();
            assert!(
                executable_grant(
                    path,
                    Path::new("/operator/rust-analyzer"),
                    &fd,
                    &path.metadata().unwrap()
                )
                .is_err()
            );
        }
    }

    #[test]
    #[ignore = "same native image, libraries and capped cgroup as the real project gate"]
    fn lsp_and_patcher_tests_rust_worker_kernel_denials() {
        let exe = PathBuf::from(std::env::var_os("EON_TEST_RUST_ANALYZER").unwrap())
            .canonicalize()
            .unwrap();
        let libs = std::env::var_os("EON_TEST_RUST_ANALYZER_RUNTIME").unwrap();
        let group = match std::env::var("EON_TEST_RUST_ANALYZER_CGROUP")
            .unwrap()
            .as_str()
        {
            "self" => Path::new("/sys/fs/cgroup").join(
                std::fs::read_to_string("/proc/self/cgroup")
                    .unwrap()
                    .trim()
                    .strip_prefix("0::/")
                    .unwrap(),
            ),
            path => PathBuf::from(path),
        };
        let mut read_only: Vec<_> = std::env::split_paths(&libs)
            .map(|p| p.canonicalize().unwrap())
            .collect();
        read_only.push(exe.clone());
        let mut cmd = Command::new(&exe);
        confine_command(
            &mut cmd,
            &ConfinementPolicy {
                read_only,
                deny_network: true,
            },
            WorkerIo::RustAnalyzer(&group),
        )
        .unwrap();
        let image = CString::new(exe.as_os_str().as_bytes()).unwrap();
        let unshared = CString::new(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
        // SAFETY: child-only negative probes *after* both production filters.
        // Never connect a socket, truncate a file or signal another process.
        unsafe {
            cmd.pre_exec(move || {
                let denied = |ret, errno| {
                    if ret == -1 && io::Error::last_os_error().raw_os_error() == Some(errno) {
                        Ok(())
                    } else {
                        Err(io::Error::from_raw_os_error(libc::EINVAL))
                    }
                };
                for nr in [libc::SYS_fork, libc::SYS_clone] {
                    let r = libc::syscall(nr, libc::SIGCHLD, 0usize, 0usize, 0usize, 0usize);
                    if r == 0 {
                        libc::_exit(1);
                    } // unexpected process in a regressed filter
                    denied(r, libc::EPERM)?;
                }
                denied(
                    libc::syscall(libc::SYS_clone3, 0usize, 0usize),
                    libc::ENOSYS,
                )?;
                denied(
                    libc::syscall(libc::SYS_socket, libc::AF_INET, libc::SOCK_STREAM, 0),
                    libc::EPERM,
                )?;
                denied(
                    libc::syscall(libc::SYS_kill, libc::getpid(), 0),
                    libc::EPERM,
                )?;
                denied(
                    libc::syscall(
                        libc::SYS_openat,
                        libc::AT_FDCWD,
                        image.as_ptr(),
                        libc::O_WRONLY,
                    ),
                    libc::EPERM,
                )?;
                denied(
                    libc::syscall(libc::SYS_write, 3, 0usize, 0usize),
                    libc::EPERM,
                )?;
                denied(
                    libc::syscall(
                        libc::SYS_openat,
                        libc::AT_FDCWD,
                        unshared.as_ptr(),
                        libc::O_RDONLY,
                    ),
                    libc::EACCES,
                )?;
                let args: [*const libc::c_char; 2] = [c"true".as_ptr(), std::ptr::null()];
                denied(
                    i64::from(libc::execve(
                        c"/usr/bin/true".as_ptr(),
                        args.as_ptr(),
                        args[1..].as_ptr(),
                    )),
                    libc::EACCES,
                )
            });
        }
        let mut child = cmd.spawn().unwrap();
        // Keep stdin open until inspection completes. Group membership and hard
        // limits are independently observable outside the confined process.
        let proc = Path::new("/proc").join(child.id().to_string());
        let image = (proc.join("exe")).read_link();
        let limits = std::fs::read_to_string(proc.join("limits"));
        let members = std::fs::read_to_string(group.join("cgroup.procs"));
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(image.unwrap(), exe);
        assert!(
            members
                .unwrap()
                .lines()
                .any(|id| id == child.id().to_string())
        );
        let limits = limits.unwrap();
        for (prefix, cap) in [
            ("Max cpu time", 60),
            ("Max open files", 64),
            ("Max address space", 2 * 1024 * 1024 * 1024_u64),
            ("Max core file size", 0),
            ("Max file size", 0),
        ] {
            let values = limits
                .lines()
                .find_map(|s| s.strip_prefix(prefix))
                .unwrap()
                .split_whitespace();
            assert!(values.take(2).all(|s| s.parse::<u64>().unwrap() <= cap));
        }
    }
}
