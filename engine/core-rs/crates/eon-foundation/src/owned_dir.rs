//! Directory capability for local durable state. On Linux, acquire one component at a
//! time without following links. Operations through `path()` resolve from the held fd,
//! even if the original name is replaced. Root and the effective UID are trusted.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

pub struct OwnedDirectory {
    #[cfg(target_os = "linux")]
    _directory: std::fs::File,
    path: PathBuf,
}

impl OwnedDirectory {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Create a private lock file, or re-open an owned single-link regular file.
    /// Never truncates, and never opens a device or FIFO for I/O on Linux.
    pub fn lock_file(&self, name: &std::ffi::OsStr) -> io::Result<File> {
        if Path::new(name).file_name() != Some(name) {
            return Err(io::Error::other("expected a single lock file name"));
        }
        open_or_create(&self.path.join(name))
    }

    #[cfg(target_os = "linux")]
    pub fn open(path: &Path, create: bool) -> io::Result<Self> {
        use rustix::fs::{Mode, OFlags, mkdirat, open, openat};
        use rustix::io::Errno;
        use std::fs::File;
        use std::os::unix::fs::MetadataExt;
        use std::path::Component;

        let flags = OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
        let absolute = std::path::absolute(path)?;
        let uid = rustix::process::geteuid().as_raw();
        let mut dir: File = open("/", flags, Mode::empty())?.into();
        for component in absolute.components() {
            let name = match component {
                Component::RootDir | Component::CurDir => continue,
                Component::Normal(name) => name,
                _ => return Err(io::Error::other("parent traversal in state path")),
            };
            let m = dir.metadata()?;
            // A root-owned sticky ancestor (e.g. /tmp) cannot replace our or root's
            // children. Reject attacker-owned children on the following iteration.
            if (m.uid() != uid && m.uid() != 0)
                || (m.mode() & 0o022 != 0 && !(m.uid() == 0 && m.mode() & 0o1000 != 0))
            {
                return Err(io::Error::other("untrusted state directory ancestor"));
            }

            let next = match openat(&dir, name, flags, Mode::empty()) {
                Err(Errno::NOENT) if create => {
                    match mkdirat(&dir, name, Mode::RWXU) {
                        Ok(()) | Err(Errno::EXIST) => {}
                        Err(e) => return Err(e.into()),
                    }
                    openat(&dir, name, flags, Mode::empty())?
                }
                result => result?,
            };
            dir = next.into();
        }
        Self::from_fd(dir)
    }

    #[cfg(target_os = "linux")]
    fn from_fd(dir: std::fs::File) -> io::Result<Self> {
        use std::os::{fd::AsRawFd, unix::fs::MetadataExt};
        let uid = rustix::process::geteuid().as_raw();
        let m = dir.metadata()?;
        if m.uid() != uid || m.mode() & 0o022 != 0 {
            return Err(io::Error::other(
                "state directory not controlled by effective UID",
            ));
        }
        let path = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()));
        Ok(Self {
            _directory: dir,
            path,
        })
    }

    #[cfg(not(target_os = "linux"))]
    pub fn open(path: &Path, create: bool) -> io::Result<Self> {
        if create {
            std::fs::create_dir_all(path)?;
        }
        Ok(Self {
            path: std::fs::canonicalize(path)?,
        })
    }

    /// Pin an immediate subdirectory of this handle, without re-resolving its ancestry.
    pub fn child(&self, name: &str) -> io::Result<Self> {
        if Path::new(name).file_name() != Some(std::ffi::OsStr::new(name)) {
            return Err(io::Error::other("expected a single directory name"));
        }
        #[cfg(target_os = "linux")]
        {
            use rustix::fs::{Mode, OFlags, openat};
            let flags = OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
            Self::from_fd(openat(&self._directory, name, flags, Mode::empty())?.into())
        }
        #[cfg(not(target_os = "linux"))]
        Self::open(&self.path.join(name), false)
    }
}

// O_PATH never invokes a device/FIFO open handler. Reopen only the pinned inode.
pub(crate) fn open_regular(path: &Path, opts: &OpenOptions) -> io::Result<File> {
    #[cfg(target_os = "linux")]
    {
        use rustix::fs::{Mode, OFlags, open};
        use std::os::{fd::AsRawFd, unix::fs::MetadataExt};
        let pin: File = open(
            path,
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?
        .into();
        let meta = pin.metadata()?;
        if !meta.is_file()
            || meta.nlink() != 1
            || meta.uid() != rustix::process::geteuid().as_raw()
            || meta.mode() & 0o022 != 0
        {
            return Err(io::Error::other("untrusted or linked state file"));
        }
        opts.open(format!("/proc/self/fd/{}", pin.as_raw_fd()))
    }
    #[cfg(not(target_os = "linux"))]
    opts.open(path)
}

pub(crate) fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
}

pub(crate) fn open_or_create(path: &Path) -> io::Result<File> {
    match private_options()
        .create_new(true)
        .read(true)
        .write(true)
        .open(path)
    {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            open_regular(path, OpenOptions::new().read(true).write(true))
        }
        result => result,
    }
}
