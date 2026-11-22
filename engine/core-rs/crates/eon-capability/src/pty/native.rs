//! Shared devpts acquisition and terminal lifecycle; only child images and
//! confinement differ between Linux/glibc and Android/Bionic.
#[cfg(target_os = "android")]
mod android;
#[cfg(target_os = "android")]
pub(super) use android::Programs;
#[cfg(target_os = "linux")]
mod host;
#[cfg(target_os = "linux")]
use host::host_elf;
#[cfg(feature = "portable-pty-allocator")]
#[path = "allocator/grant.rs"]
mod grant;
#[cfg(feature = "portable-pty-allocator")]
mod portable;

use std::fs::File;
use std::io::{self, ErrorKind, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::process::CommandExt;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use rustix::fs;
#[cfg(any(not(feature = "portable-pty-allocator"), test))]
use rustix::fs::Mode;
#[cfg(target_os = "linux")]
use rustix::process::Signal;
use rustix::process::{self, Resource, Rlimit};

use super::{PtyMode, PtySignal};
#[cfg(target_os = "linux")]
use crate::sandbox::{ConfinementPolicy, WorkerIo, confine_command};

fn fd_path(f: &File) -> PathBuf {
    format!("/proc/self/fd/{}", f.as_raw_fd()).into()
}

#[cfg(target_os = "linux")]
pub(super) struct Image {
    utility: Utility,
    program: File,
    libc: File,
    loader: File,
}

#[derive(Clone, Copy)]
#[cfg(target_os = "linux")]
pub(super) enum Utility {
    Cat,
    Tr,
    Wc,
}

#[cfg(target_os = "linux")]
impl Utility {
    fn name(self) -> &'static str {
        match self {
            Self::Cat => "cat",
            Self::Tr => "tr",
            Self::Wc => "wc",
        }
    }

    fn pin_program(self) -> io::Result<File> {
        let bin = Path::new("/usr/bin");
        // Ubuntu's split coreutils packages install dedicated GNU executables
        // under gnu-prefixed names. The unprefixed selectors can point at a
        // multicall binary. Never follow them or grant that binary's runtime.
        host_program(
            &bin.join(format!("gnu{}", self.name())),
            &bin.join(self.name()),
        )
    }
}

#[cfg(target_os = "linux")]
fn host_program(split: &Path, classic: &Path) -> io::Result<File> {
    let program = match host_elf(split) {
        Err(e) if e.kind() == ErrorKind::NotFound => host_elf(classic)?,
        result => result?,
    };
    if program.metadata()?.mode() & 0o111 == 0 {
        return Err(io::Error::from(ErrorKind::PermissionDenied));
    }
    Ok(program)
}

#[cfg(all(test, target_os = "linux"))]
#[test]
fn only_a_missing_split_image_allows_a_classic_image() {
    let split = Path::new("/usr/bin/gnucat");
    let regular = if split.try_exists().unwrap() {
        split
    } else {
        Path::new("/usr/bin/cat")
    };
    // Controlled missing entry, symlink and non-regular file in trusted dirs;
    // no privileges, system installation changes or fixture execution.
    let missing = Path::new("/proc/eon-no-such-entry");
    let link = Path::new("/proc/self");
    let pinned = host_program(missing, regular).unwrap().metadata().unwrap();
    let expected = regular.symlink_metadata().unwrap();
    assert_eq!(
        (pinned.dev(), pinned.ino()),
        (expected.dev(), expected.ino())
    );
    for (first, second) in [(link, regular), (missing, link)] {
        assert_eq!(
            host_program(first, second).unwrap_err().raw_os_error(),
            Some(libc::ELOOP)
        );
    }
    assert_eq!(
        host_program(Path::new("/"), regular).unwrap_err().kind(),
        ErrorKind::PermissionDenied
    );
}

#[cfg(target_os = "linux")]
impl Image {
    pub(super) fn pin(utility: Utility) -> io::Result<Self> {
        if fs::fstatfs(File::open("/proc/self/fd")?)?.f_type != fs::PROC_SUPER_MAGIC {
            return Err(io::Error::new(
                ErrorKind::Unsupported,
                "PTY requires procfs",
            ));
        }
        let program = utility.pin_program()?;
        let (multiarch, ld) = match std::env::consts::ARCH {
            "x86_64" => ("x86_64-linux-gnu", "ld-linux-x86-64.so.2"),
            "aarch64" => ("aarch64-linux-gnu", "ld-linux-aarch64.so.1"),
            "riscv64" => ("riscv64-linux-gnu", "ld-linux-riscv64-lp64d.so.1"),
            _ => {
                return Err(io::Error::new(
                    ErrorKind::Unsupported,
                    "unsupported native PTY ABI",
                ));
            }
        };
        // Only individual files in the host glibc installation. Neither the
        // workspace nor /etc, /proc, /usr or a library tree is readable.
        let lib = Path::new("/usr/lib");
        for dir in [lib.join(multiarch), "/usr/lib64".into(), lib.into()] {
            let pin_lib = |name| host_elf(&dir.join(name));
            match (pin_lib("libc.so.6"), pin_lib(ld)) {
                (Ok(libc), Ok(loader)) => {
                    return Ok(Self {
                        utility,
                        program,
                        libc,
                        loader,
                    });
                }
                (Err(e), _) | (_, Err(e)) if e.kind() != ErrorKind::NotFound => return Err(e),
                _ => continue,
            }
        }
        Err(io::Error::new(
            ErrorKind::Unsupported,
            "missing trusted PTY runtime",
        ))
    }

    fn command(&self, args: &[&'static str]) -> io::Result<Command> {
        let mut cmd = Command::new(fd_path(&self.program));
        cmd.arg0(self.utility.name()).args(args);
        Ok(cmd)
    }

    fn confine(&self, cmd: &mut Command) -> io::Result<()> {
        let policy = ConfinementPolicy {
            read_only: vec![
                fd_path(&self.program),
                fd_path(&self.loader),
                fd_path(&self.libc),
            ],
            deny_network: true,
        };
        // SAFETY: child-only rlimit syscalls, before installing seccomp.
        unsafe {
            cmd.pre_exec(|| {
                limit(Resource::Core, 0)?;
                limit(Resource::As, 64 * 1024 * 1024)
            });
        }
        confine_command(cmd, &policy, WorkerIo::StdioProtocol).map_err(io::Error::other)
    }

    pub(super) fn batch(&self, args: &[&'static str], max_file: u64) -> io::Result<Command> {
        require_waitable_children()?;
        let mut cmd = self.command(args)?;
        let parent = process::getpid();
        // SAFETY: child-only syscalls before confinement; the synchronous batch
        // child must not outlive its calling thread if the host exits abruptly.
        unsafe {
            cmd.pre_exec(move || {
                process::set_parent_process_death_signal(Some(Signal::KILL))?;
                if process::getppid() != Some(parent) {
                    return Err(io::Error::from_raw_os_error(libc::ESRCH));
                }
                limit(Resource::Fsize, max_file)
            });
        }
        self.confine(&mut cmd)?;
        Ok(cmd)
    }
}

#[cfg(target_os = "linux")]
pub(super) struct Programs {
    cat: Image,
    tr: Image,
    #[cfg(feature = "portable-pty-allocator")]
    grant: grant::Grant,
}

#[cfg(target_os = "linux")]
impl Programs {
    pub(super) fn pin() -> io::Result<Self> {
        Ok(Self {
            cat: Image::pin(Utility::Cat)?,
            tr: Image::pin(Utility::Tr)?,
            #[cfg(feature = "portable-pty-allocator")]
            grant: grant::Grant::pin()?,
        })
    }

    pub(super) fn launch(&self, mode: PtyMode) -> io::Result<Terminal> {
        let (image, args): (_, &[_]) = match mode {
            PtyMode::Loopback => (&self.cat, &[]),
            PtyMode::Filter(filter) => (&self.tr, filter.args()),
        };
        #[cfg(feature = "portable-pty-allocator")]
        let acquire = || portable::acquire(&self.grant);
        #[cfg(not(feature = "portable-pty-allocator"))]
        let acquire = private_locked_pair;
        spawn_terminal(mode, image.command(args)?, acquire, |cmd| {
            image.confine(cmd)
        })
    }
}

fn require_waitable_children() -> io::Result<()> {
    // An auto-reaped leader cannot pin its process-group ID for cleanup.
    // SAFETY: null act is a read-only query; success initializes previous.
    let previous = unsafe {
        let mut previous = MaybeUninit::uninit();
        cvt(libc::sigaction(
            libc::SIGCHLD,
            std::ptr::null(),
            previous.as_mut_ptr(),
        ))?;
        previous.assume_init()
    };
    if previous.sa_sigaction == libc::SIG_IGN || previous.sa_flags & libc::SA_NOCLDWAIT != 0 {
        return Err(io::Error::other(
            "provider requires waitable child processes",
        ));
    }
    Ok(())
}

fn spawn_terminal(
    mode: PtyMode,
    mut cmd: Command,
    acquire: impl FnOnce() -> io::Result<(File, File)>,
    confine: impl FnOnce(&mut Command) -> io::Result<()>,
) -> io::Result<Terminal> {
    require_waitable_children()?;
    let (master, slave) = pair(acquire()?)?;
    // SAFETY: in the child stdio is already installed. setsid makes a new
    // process group and session before claiming the slave as controlling
    // terminal. Only async-signal-safe native operations before confinement.
    unsafe {
        cmd.pre_exec(|| {
            cvt(libc::setsid())?;
            cvt(libc::ioctl(0, libc::TIOCSCTTY, 0))?;
            let mut signals = MaybeUninit::uninit();
            cvt(libc::sigemptyset(signals.as_mut_ptr()))?;
            let mut signals = signals.assume_init();
            for sig in [libc::SIGINT, libc::SIGTERM] {
                if libc::signal(sig, libc::SIG_DFL) == libc::SIG_ERR {
                    return Err(io::Error::last_os_error());
                }
                cvt(libc::sigaddset(&mut signals, sig))?;
            }
            cvt(libc::sigprocmask(
                libc::SIG_UNBLOCK,
                &signals,
                std::ptr::null_mut(),
            ))?;
            Ok(())
        });
    }
    confine(&mut cmd)?;
    // Replace just the default stdio pipes, retaining all confinement hooks,
    // the empty environment and the fixed working directory.
    cmd.stdin(slave.try_clone()?)
        .stdout(slave.try_clone()?)
        .stderr(slave);
    let mut terminal = Terminal {
        master,
        child: cmd.spawn()?,
    };
    drop(cmd);
    let (probe, reply) = mode.probe();
    terminal.verify(probe, reply)?;
    Ok(terminal)
}

fn limit(resource: Resource, cap: u64) -> io::Result<()> {
    // Never raise an inherited limit, even if the host has the privilege to do so.
    let value = Some(process::getrlimit(resource).current.unwrap_or(cap).min(cap));
    Ok(process::setrlimit(
        resource,
        Rlimit {
            current: value,
            maximum: value,
        },
    )?)
}

fn cvt(n: libc::c_int) -> io::Result<libc::c_int> {
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n)
    }
}

const TERMINAL_FLAGS: i32 = libc::O_RDWR | libc::O_CLOEXEC | libc::O_NOCTTY;

#[cfg(target_os = "linux")]
pub(super) use libc::{TIOCGPTLCK, TIOCGPTN, TIOCGPTPEER, TIOCSPTLCK};
// Android's libc crate omits these asm-generic/ioctls.h request names.
#[cfg(target_os = "android")]
pub(super) const TIOCGPTLCK: libc::Ioctl = libc::_IOR::<i32>(b'T' as _, 0x39);
#[cfg(target_os = "android")]
pub(super) const TIOCGPTN: libc::Ioctl = libc::_IOR::<u32>(b'T' as _, 0x30);
#[cfg(target_os = "android")]
const TIOCGPTPEER: libc::Ioctl = libc::_IO(b'T' as _, 0x41);
#[cfg(target_os = "android")]
const TIOCSPTLCK: libc::Ioctl = libc::_IOW::<i32>(b'T' as _, 0x31);

fn peer(master: &File, flags: i32) -> io::Result<File> {
    // SAFETY: on success the ioctl returns a new descriptor for this exact
    // master's peer on its actual devpts mount, without resolving a /dev path.
    unsafe {
        let fd = cvt(libc::ioctl(master.as_raw_fd(), TIOCGPTPEER, flags))?;
        Ok(File::from_raw_fd(fd))
    }
}

#[cfg(any(not(feature = "portable-pty-allocator"), test))]
pub(super) fn private_locked_pair() -> io::Result<(File, File)> {
    // portable-pty 0.9's Unix openpty returns an already-unlocked slave and
    // sets CLOEXEC afterwards. Neither is safe to repair after acquisition.
    // SAFETY: posix_openpt returns an owned master; the two query ioctls
    // initialize the supplied integers without changing the new slave's lock.
    let (master, number) = unsafe {
        let fd = cvt(libc::posix_openpt(TERMINAL_FLAGS | libc::O_NONBLOCK))?;
        let master = File::from_raw_fd(fd);
        let mut locked: libc::c_int = 0;
        let mut number: libc::c_uint = 0;
        cvt(libc::ioctl(fd, TIOCGPTLCK, &mut locked))?;
        cvt(libc::ioctl(fd, TIOCGPTN, &mut number))?;
        if locked != 1 {
            return Err(io::Error::from(ErrorKind::PermissionDenied));
        }
        (master, number)
    };
    // O_PATH does not open the terminal for I/O. The slave may initially be
    // 0620 and owned by group tty. Do not unlock until it is operator-only.
    let pin = peer(&master, libc::O_PATH | libc::O_CLOEXEC)?;
    let meta = pin.metadata()?;
    let uid = rustix::process::geteuid();
    let gid = rustix::process::getegid();
    if fs::fstatfs(&pin)?.f_type != 0x1cd1 // DEVPTS_SUPER_MAGIC
        || !meta.file_type().is_char_device()
        || fs::major(meta.rdev()) != 136 // UNIX98_PTY_SLAVE_MAJOR
        || fs::minor(meta.rdev()) != number
        || meta.uid() != uid.as_raw()
    {
        return Err(io::Error::from(ErrorKind::PermissionDenied));
    }
    // fchmod cannot operate on O_PATH. Follow only the kernel's magic link
    // to our live descriptor, relative to a verified procfs directory.
    #[cfg(target_os = "linux")]
    {
        let procfd = File::open("/proc/self/fd")?;
        if fs::fstatfs(&procfd)?.f_type != fs::PROC_SUPER_MAGIC {
            return Err(io::Error::from(ErrorKind::PermissionDenied));
        }
        fs::chmodat(
            &procfd,
            pin.as_raw_fd().to_string(),
            Mode::RUSR | Mode::WUSR,
            fs::AtFlags::empty(),
        )?;
        fs::chownat(&pin, "", None, Some(gid), fs::AtFlags::EMPTY_PATH)?;
    }
    // Stock Android allocates in mode 0600 with the operator's uid/gid.
    // Do not attempt to fix device policy on a nonconforming mount.
    let meta = pin.metadata()?;
    if meta.mode() & 0o7777 != 0o600 || meta.uid() != uid.as_raw() || meta.gid() != gid.as_raw() {
        return Err(io::Error::from(ErrorKind::PermissionDenied));
    }
    Ok((master, pin))
}

fn open_private_slave(master: &File) -> io::Result<File> {
    // SAFETY: the live master is ours; TIOCSPTLCK reads a single integer.
    unsafe {
        let fd = master.as_raw_fd();
        cvt(libc::unlockpt(fd))?;
        let opened = peer(master, TERMINAL_FLAGS);
        // Even a failed open (e.g. EMFILE) must re-lock before returning:
        // an in-flight host fork can still hold a copy of the master.
        cvt(libc::ioctl(fd, TIOCSPTLCK, &1_i32))?;
        opened
    }
}

fn pair((master, pin): (File, File)) -> io::Result<(File, File)> {
    let slave = open_private_slave(&master)?;
    let expected = pin.metadata()?;
    let actual = slave.metadata()?;
    if (actual.dev(), actual.ino()) != (expected.dev(), expected.ino()) {
        return Err(io::Error::from(ErrorKind::PermissionDenied));
    }
    // SAFETY: peer is still open; successful tcgetattr initializes attr.
    unsafe {
        let peer = slave.as_raw_fd();
        let mut attr = MaybeUninit::uninit();
        cvt(libc::tcgetattr(peer, attr.as_mut_ptr()))?;
        let mut attr = attr.assume_init();
        libc::cfmakeraw(&mut attr); // No line-discipline echo, signals, or byte translations.
        cvt(libc::tcsetattr(peer, libc::TCSANOW, &attr))?;
        resize(peer, 24, 80)?;
        Ok((master, slave))
    }
}

#[cfg(test)]
#[test]
fn peer_acquisition_relocks_on_fd_exhaustion_and_uses_cloexec() {
    let (master, pin) = private_locked_pair().unwrap();
    let child_master = master.try_clone().unwrap();
    // Lower the descriptor limit only in the forked child. Its error goes
    // through Command's existing error pipe; nothing is executed.
    let mut child = Command::new("/");
    // SAFETY: child-only rlimit and PTY syscalls on an owned descriptor.
    unsafe {
        child.pre_exec(move || {
            limit(Resource::Nofile, 0)?;
            open_private_slave(&child_master)?;
            Err(io::Error::from_raw_os_error(libc::EINVAL))
        });
    }
    assert_eq!(
        child.spawn().unwrap_err().raw_os_error(),
        Some(libc::EMFILE)
    );
    let mut locked: libc::c_int = 0;
    // SAFETY: TIOCGPTLCK writes an integer for this same live master.
    assert_eq!(
        unsafe { libc::ioctl(master.as_raw_fd(), TIOCGPTLCK, &mut locked) },
        0
    );
    assert_eq!(locked, 1);
    let slave = open_private_slave(&master).unwrap();
    for fd in [&master, &pin, &slave] {
        assert!(
            rustix::io::fcntl_getfd(fd)
                .unwrap()
                .contains(rustix::io::FdFlags::CLOEXEC)
        );
    }
    assert!(
        fs::fcntl_getfl(&master)
            .unwrap()
            .contains(fs::OFlags::NONBLOCK)
    );
    assert!(fs::fcntl_getfl(&pin).unwrap().contains(fs::OFlags::PATH));
}

fn resize(fd: i32, rows: u16, cols: u16) -> io::Result<()> {
    if rows == 0 || cols == 0 {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "zero PTY dimension",
        ));
    }
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCSWINSZ reads this initialized winsize while the fd is live.
    unsafe {
        cvt(libc::ioctl(fd, libc::TIOCSWINSZ, &size))?;
    }
    Ok(())
}

pub(super) struct Terminal {
    pub(super) master: File,
    child: Child,
}

impl Terminal {
    // exec success alone doesn't mean the loader or filter worked. Consume the
    // entire private response (possibly split across reads) with kernel echo off.
    fn verify(&mut self, probe: &[u8], mut reply: &[u8]) -> io::Result<()> {
        self.master.write_all(probe)?;
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut pfd = libc::pollfd {
            fd: self.master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        while !reply.is_empty() {
            let ms = deadline
                .saturating_duration_since(Instant::now())
                .as_millis() as i32;
            if ms == 0 {
                return Err(io::Error::from(ErrorKind::TimedOut));
            }
            // SAFETY: pfd is a single initialized pollfd for the live master.
            match cvt(unsafe { libc::poll(&mut pfd, 1, ms) }) {
                Ok(0) => return Err(io::Error::from(ErrorKind::TimedOut)),
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                result => {
                    result?;
                }
            }
            let mut answer = [0; 8];
            match self.master.read(&mut answer) {
                Ok(n) if n != 0 && reply.starts_with(&answer[..n]) => reply = &reply[n..],
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {
                    continue;
                }
                _ => return Err(io::Error::other("confined PTY failed its startup probe")),
            }
        }
        Ok(())
    }

    fn signal_group(&self, sig: i32) -> io::Result<()> {
        // SAFETY: setsid established a private group; the leader is our child
        // and has not been waited on. The host must not independently reap it.
        unsafe {
            cvt(libc::kill(-(self.child.id() as i32), sig))?;
        }
        Ok(())
    }

    pub(super) fn signal(&self, sig: PtySignal) -> io::Result<()> {
        match sig {
            PtySignal::Interrupt => self.signal_group(libc::SIGINT),
            PtySignal::Terminate => self.signal_group(libc::SIGTERM),
            PtySignal::Resize { rows, cols } => resize(self.master.as_raw_fd(), rows, cols),
        }
    }

    #[cfg(test)]
    pub(super) fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.signal_group(libc::SIGKILL);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
