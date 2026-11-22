use std::fs::File;
use std::io::{self, Read, Seek, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rustix::fs::{
    MemfdFlags, Mode, OFlags, SealFlags, fcntl_add_seals, memfd_create, open, openat,
};
use rustix::process::{self, Pid, Resource, Signal, WaitId, WaitIdOptions};

use super::{
    EonError, EonResult, MAX_OUTPUT_BYTES, OUTPUT, RUN, VALIDATE, WasmWorker, worker_error,
};

const ADDRESS_SPACE: u64 = 512 * 1024 * 1024;
const CPU_SECONDS: u64 = 5;

pub(super) fn open_executable(path: &Path) -> io::Result<File> {
    let uid = process::geteuid().as_raw();
    let flags = OFlags::PATH | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let mut pin = File::from(open("/", flags | OFlags::DIRECTORY, Mode::empty())?);
    for component in path.components() {
        let name = match component {
            Component::RootDir => continue,
            Component::Normal(name) => name,
            _ => return Err(io::Error::other("invalid worker path component")),
        };
        let meta = pin.metadata()?;
        if !meta.is_dir() || (meta.uid() != 0 && meta.uid() != uid) || meta.mode() & 0o022 != 0 {
            return Err(io::Error::other("unprotected worker directory"));
        }
        pin = openat(&pin, name, flags, Mode::empty())?.into();
    }
    // O_PATH doesn't invoke device/FIFO open handlers. Inspect the leaf and
    // the ELF magic via the same inode we'll later exec, never re-resolve path.
    let meta = pin.metadata()?;
    if !meta.is_file()
        || meta.nlink() != 1
        || (meta.uid() != 0 && meta.uid() != uid)
        || meta.mode() & 0o6222 != 0
        || meta.mode() & 0o111 == 0
        || pin.as_raw_fd() <= 2
    {
        return Err(io::Error::other(
            "expected protected read-only worker executable",
        ));
    }
    let mut file = File::open(format!("/proc/self/fd/{}", pin.as_raw_fd()))?;
    let mut magic = [0; 4];
    file.read_exact(&mut magic)?;
    if &magic != b"\x7fELF" {
        return Err(io::Error::other("expected native ELF worker"));
    }
    Ok(pin)
}

// Unlike a pipe write, preparing this bounded request can't block waiting for
// the worker. Sealing also prevents the child enlarging the anonymous file.
fn request(module: &[u8], input: Option<&str>) -> io::Result<File> {
    let mut file = File::from(memfd_create(
        c"eon-wasm",
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
    )?);
    file.write_all(&(module.len() as u32).to_le_bytes())?;
    file.write_all(module)?;
    file.write_all(input.unwrap_or("").as_bytes())?;
    file.rewind()?;
    fcntl_add_seals(
        &file,
        SealFlags::WRITE | SealFlags::GROW | SealFlags::SHRINK | SealFlags::SEAL,
    )?;
    Ok(file)
}

fn limit(resource: Resource, cap: u64) -> io::Result<()> {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: valid resource and stack rlimit; never raise an inherited limit.
    unsafe {
        if libc::getrlimit(resource as _, &mut lim) < 0 {
            return Err(io::Error::last_os_error());
        }
        lim.rlim_cur = lim.rlim_cur.min(cap as libc::rlim_t);
        lim.rlim_max = lim.rlim_cur;
        if libc::setrlimit(resource as _, &lim) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

struct WorkerChild {
    child: Child,
    lost_child: bool,
}

impl WorkerChild {
    fn pid(&self) -> Pid {
        Pid::from_child(&self.child)
    }

    fn wait_success(&mut self, deadline: Instant) -> io::Result<()> {
        // Keep the leader waitable until cleanup, so its process-group ID is
        // not recycled between observing exit and signalling the group.
        let flags = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
        loop {
            let left = remaining(deadline)?;
            match process::waitid(WaitId::Pid(self.pid()), flags) {
                Ok(Some(status)) if status.exit_status() == Some(0) => return Ok(()),
                Ok(Some(_)) => return Err(io::Error::other("worker did not exit successfully")),
                Ok(None) => std::thread::sleep(left.min(Duration::from_millis(1))),
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => {
                    self.lost_child = e == rustix::io::Errno::CHILD;
                    return Err(e.into());
                }
            }
        }
    }
}

impl Drop for WorkerChild {
    fn drop(&mut self) {
        // The embedder must not compete for wait statuses. If the kernel
        // nevertheless reports no such child, do not signal numeric IDs.
        if self.lost_child
            || matches!(
                process::waitid(
                    WaitId::Pid(self.pid()),
                    WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT
                ),
                Err(rustix::io::Errno::CHILD)
            )
        {
            return;
        }
        let _ = process::kill_process_group(self.pid(), Signal::KILL);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn(exe: &File, args: &[&str], stdin: File, stdout: UnixStream) -> io::Result<WorkerChild> {
    // Auto-reaping would release the group leader's PID before teardown.
    // SAFETY: null act only queries the disposition into a correctly sized buffer.
    let action = unsafe {
        let mut action = std::mem::MaybeUninit::uninit();
        if libc::sigaction(libc::SIGCHLD, std::ptr::null(), action.as_mut_ptr()) < 0 {
            return Err(io::Error::last_os_error());
        }
        action.assume_init()
    };
    if action.sa_sigaction == libc::SIG_IGN || action.sa_flags & libc::SA_NOCLDWAIT != 0 {
        return Err(io::Error::other("Wasm workers require waitable children"));
    }
    let mut cmd = Command::new(format!("/proc/self/fd/{}", exe.as_raw_fd()));
    cmd.args(args)
        .env_clear()
        .current_dir("/")
        .process_group(0)
        .stdin(stdin)
        .stdout(OwnedFd::from(stdout))
        .stderr(Stdio::null());
    let parent = process::getpid();
    // SAFETY: child-only syscalls with scalar/stack arguments, no locks,
    // allocation or formatting between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            process::set_parent_process_death_signal(Some(Signal::KILL))?;
            if process::getppid() != Some(parent) {
                return Err(io::Error::from_raw_os_error(libc::ESRCH));
            }
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) < 0
                || libc::syscall(
                    libc::SYS_close_range,
                    3u32,
                    u32::MAX,
                    libc::CLOSE_RANGE_CLOEXEC,
                ) < 0
            {
                return Err(io::Error::last_os_error());
            }
            limit(Resource::Core, 0)?;
            limit(Resource::Cpu, CPU_SECONDS)?;
            limit(Resource::As, ADDRESS_SPACE)
        });
    }
    Ok(WorkerChild {
        child: cmd.spawn()?,
        lost_child: false,
    })
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(io::ErrorKind::TimedOut.into());
    }
    Ok(left)
}

fn supervise(worker: &WasmWorker, module: &[u8], input: Option<&str>) -> io::Result<Vec<u8>> {
    let deadline = Instant::now() + worker.wall_time;
    let (reader, writer) = UnixStream::pair()?;
    let (arg, max_reply) = if input.is_some() {
        (RUN, OUTPUT.len() + MAX_OUTPUT_BYTES)
    } else {
        (VALIDATE, 64)
    };
    let request = request(module, input)?;
    remaining(deadline)?;
    let child = spawn(&worker.executable, &[arg], request, writer)?;
    read_reply(child, reader, deadline, max_reply)
}

fn read_reply(
    mut child: WorkerChild,
    mut reader: UnixStream,
    deadline: Instant,
    max_reply: usize,
) -> io::Result<Vec<u8>> {
    let mut reply = Vec::new();
    let mut buf = [0; 4096];
    loop {
        reader.set_read_timeout(Some(remaining(deadline)?))?;
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) if reply.len() + n <= max_reply => reply.extend_from_slice(&buf[..n]),
            Ok(_) => return Err(io::Error::other("oversized worker response")),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    child.wait_success(deadline)?;
    remaining(deadline)?;
    Ok(reply)
}

pub(super) fn exchange(
    worker: &WasmWorker,
    module: &[u8],
    input: Option<&str>,
) -> EonResult<Vec<u8>> {
    supervise(worker, module, input).map_err(|e| {
        if e.kind() == io::ErrorKind::TimedOut {
            EonError::ResourceLeaseTimeout {
                lease_id: "wasm.worker".into(),
            }
        } else {
            worker_error(e)
        }
    })
}

#[cfg(test)]
mod tests;
