//! One captured extent per branch. No source path, link, cwd or executable
//! directory capability crosses this boundary. Only a selected export names its data.
use std::fs::{File, Metadata};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::sync::Arc;

use eon_primitives::{EonResult, TaskId};
use rustix::fs::{self, AtFlags, Mode, OFlags, ResolveFlags};
use rustix::io::Errno;

use super::workspace::failed;
use super::{invalid, public_path};

const DIR: OFlags = OFlags::PATH
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// A pinned, operator-owned 0700 scratch directory supplied independently of the
/// intent. The trusted host must control this directory exclusively, keep it
/// outside the source workspace (including bind-mount aliases), and not move it
/// or manipulate staged entries. This is not a sandbox against the host UID.
#[derive(Clone)]
pub struct WorktreeStore(Arc<Dir>);

impl WorktreeStore {
    pub fn from_directory(directory: File) -> EonResult<Self> {
        private_dir(&directory)?;
        Ok(Self(Arc::new(Dir {
            fd: directory,
            _entry: None,
        })))
    }

    pub(super) fn check(&self, workspace: (u64, u64)) -> EonResult<()> {
        private_dir(&self.0.fd)?;
        // Refuse the workspace and descendants even if the host accidentally
        // supplied one as scratch. Only inspect pinned directory identities.
        let mut current = self.0.fd.try_clone().map_err(failed)?;
        for _ in 0..256 {
            let id = identity(&current)?;
            if id == workspace {
                return Err(invalid("worktree store is inside the source workspace"));
            }
            let parent = open_dir(&current, "..")?;
            if identity(&parent)? == id {
                return Ok(());
            }
            current = parent;
        }
        Err(invalid("worktree store ancestry too deep"))
    }

    /// Make a filesystem-local lower inode for FICLONE. Never clone the live
    /// file or the whole packed image. No capture is written to a named inode.
    pub(super) fn lower(&self, bytes: &[u8]) -> EonResult<Arc<File>> {
        let writer = self.0.anonymous_file(Mode::RUSR)?;
        writer.write_all_at(bytes, 0).map_err(failed)?;
        // dup would retain write access. Reopen the pinned inode on real procfs.
        let procfd = fs::open("/proc/self/fd", DIR, Mode::empty()).map_err(failed)?;
        if fs::fstatfs(&procfd).map_err(failed)?.f_type != fs::PROC_SUPER_MAGIC {
            return Err(invalid("trusted procfs required to reopen private extent"));
        }
        let reader = fs::openat(
            procfd,
            writer.as_raw_fd().to_string(),
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map(File::from)
        .map_err(failed)?;
        if identity(&reader)? != identity(&writer)? {
            return Err(invalid("private extent inode changed"));
        }
        Ok(Arc::new(reader))
    }
}

fn private_dir(fd: &File) -> EonResult<()> {
    let m = fd.metadata().map_err(failed)?;
    if !m.is_dir() || m.uid() != rustix::process::geteuid().as_raw() || m.mode() & 0o777 != 0o700 {
        return Err(invalid(
            "expected an operator-owned private worktree directory",
        ));
    }
    Ok(())
}

fn identity(fd: &File) -> EonResult<(u64, u64)> {
    let m = fd.metadata().map_err(failed)?;
    Ok((m.dev(), m.ino()))
}

fn open_dir(parent: &File, component: &str) -> EonResult<File> {
    fs::openat(parent, component, DIR, Mode::empty())
        .map(File::from)
        .map_err(failed)
}

// Each owned name keeps its parent alive until after unlinkat. No recursive
// removal, path retraversal or cleanup of an entry we didn't create.
struct Entry {
    parent: Arc<Dir>,
    name: String,
    flags: AtFlags,
}

impl Drop for Entry {
    fn drop(&mut self) {
        let _ = fs::unlinkat(&self.parent.fd, self.name.as_str(), self.flags);
    }
}

struct Dir {
    fd: File,
    _entry: Option<Entry>,
}

impl Dir {
    fn subdir(self: &Arc<Self>, name: &str) -> EonResult<Arc<Self>> {
        fs::mkdirat(&self.fd, name, Mode::RWXU).map_err(failed)?;
        let entry = Entry {
            parent: self.clone(),
            name: name.into(),
            flags: AtFlags::REMOVEDIR,
        };
        let fd = open_dir(&self.fd, name)?;
        private_dir(&fd)?;
        Ok(Arc::new(Dir {
            fd,
            _entry: Some(entry),
        }))
    }

    fn create_exclusive(&self, name: &str, mode: Mode) -> EonResult<File> {
        let flags = OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        fs::openat(&self.fd, name, flags | OFlags::RDWR, mode)
            .map(File::from)
            .map_err(failed)
    }

    fn anonymous_file(&self, mode: Mode) -> EonResult<File> {
        // EXCL with TMPFILE prevents even linkat from publishing the inode.
        let flags = OFlags::TMPFILE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::RDWR;
        match fs::openat(&self.fd, ".", flags, mode) {
            Ok(fd) => Ok(fd.into()),
            Err(Errno::OPNOTSUPP) => self.create_unlinked(&TaskId::new().to_string(), mode),
            Err(e) => Err(failed(e)),
        }
    }

    fn create_unlinked(&self, name: &str, mode: Mode) -> EonResult<File> {
        // A crash between create and unlink can leave only an empty file.
        private_dir(&self.fd)?;
        let fd = self.create_exclusive(name, mode)?;
        fs::unlinkat(&self.fd, name, AtFlags::empty()).map_err(failed)?;
        let m = fd.metadata().map_err(failed)?;
        if !m.is_file() || m.nlink() != 0 {
            return Err(invalid("private extent is not an unlinked regular file"));
        }
        Ok(fd)
    }
}

struct NamedFile {
    fd: File,
    entry: Entry,
}

impl NamedFile {
    fn read_only(mut self) -> EonResult<Self> {
        let reader = fs::openat(
            &self.entry.parent.fd,
            self.entry.name.as_str(),
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map(File::from)
        .map_err(failed)?;
        if identity(&reader)? != identity(&self.fd)? {
            return Err(invalid("private extent inode changed"));
        }
        self.fd = reader;
        Ok(self)
    }
}

struct Slot {
    root: Arc<Dir>,
    parent: Arc<Dir>,
    path: String,
}

impl Slot {
    fn new(store: &WorktreeStore, path: &str) -> EonResult<Self> {
        if !public_path(path) {
            return Err(invalid("invalid private worktree path"));
        }
        let root = store.0.subdir(&TaskId::new().to_string())?;
        let mut parent = root.clone();
        let mut components = path.split('/');
        components.next_back();
        for component in components {
            parent = parent.subdir(component)?;
        }
        Ok(Self {
            root,
            parent,
            path: path.into(),
        })
    }

    fn create_export(&self) -> EonResult<NamedFile> {
        let name = self.path.rsplit('/').next().unwrap();
        Ok(NamedFile {
            fd: self.parent.create_exclusive(name, Mode::RUSR)?,
            entry: Entry {
                parent: self.parent.clone(),
                name: name.into(),
                flags: AtFlags::empty(),
            },
        })
    }
}

/// The staging backend, not a claim that the selected export shares disk blocks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckoutBacking {
    Reflink,
    /// Branch directory over a read-only captured file. A clean branch reads
    /// the lower; only the first edit materializes the captured extent in it.
    FileOverlay,
}

pub(super) struct BranchTree {
    slot: Slot,
    lower: Arc<File>,
    upper: Option<File>,
    len: usize,
    poisoned: bool,
}

impl BranchTree {
    pub(super) fn new(
        store: &WorktreeStore,
        path: &str,
        lower: Arc<File>,
        len: usize,
    ) -> EonResult<(Self, CheckoutBacking)> {
        let slot = Slot::new(store, path)?;
        let upper = slot.parent.anonymous_file(Mode::RUSR | Mode::WUSR)?;
        let (upper, backend) = match fs::ioctl_ficlone(&upper, &*lower) {
            Ok(()) => (Some(upper), CheckoutBacking::Reflink),
            Err(Errno::OPNOTSUPP | Errno::NOTTY | Errno::NOSYS) => {
                (None, CheckoutBacking::FileOverlay)
            }
            Err(e) => return Err(failed(e)),
        };
        Ok((
            Self {
                slot,
                lower,
                upper,
                len,
                poisoned: false,
            },
            backend,
        ))
    }

    pub(super) fn overwrite(&mut self, start: usize, bytes: &[u8]) -> EonResult<()> {
        if self.poisoned {
            return Err(invalid("checkout write previously failed"));
        }
        if self.upper.is_none() {
            let upper = self.slot.parent.anonymous_file(Mode::RUSR | Mode::WUSR)?;
            let data = read_extent(&self.lower, self.len)?;
            upper.write_all_at(&data, 0).map_err(failed)?;
            self.upper = Some(upper);
        }
        // Never present a partially successful filesystem write.
        self.poisoned = true;
        self.upper
            .as_ref()
            .unwrap()
            .write_all_at(bytes, start as u64)
            .map_err(failed)?;
        self.poisoned = false;
        Ok(())
    }

    pub(super) fn freeze(self) -> EonResult<Vec<u8>> {
        if self.poisoned {
            return Err(invalid("checkout write previously failed"));
        }
        let fd = self.upper.as_ref().unwrap_or(&self.lower);
        read_extent(fd, self.len)
    }
}

fn read_extent(fd: &File, len: usize) -> EonResult<Vec<u8>> {
    if fd.metadata().map_err(failed)?.len() != len as u64 {
        return Err(invalid("private extent length changed"));
    }
    let mut bytes = vec![0; len];
    fd.read_exact_at(&mut bytes, 0).map_err(failed)?;
    Ok(bytes)
}

/// Read-only capability for a real directory containing just the selected
/// relative file and its parents. Byte zero is the *start of the capture*, not
/// necessarily the start of the source file. No ambient path, raw dirfd, edit,
/// subprocess or writeback interface. Drop unlinks the file and directories.
pub struct CheckoutDirectory {
    slot: Slot,
    file: NamedFile,
    len: usize,
}

impl CheckoutDirectory {
    pub(super) fn export(store: &WorktreeStore, path: &str, bytes: &[u8]) -> EonResult<Self> {
        let slot = Slot::new(store, path)?;
        let file = slot.create_export()?;
        file.fd.write_all_at(bytes, 0).map_err(failed)?;
        Ok(Self {
            slot,
            file: file.read_only()?,
            len: bytes.len(),
        })
    }

    pub fn metadata(&self) -> EonResult<Metadata> {
        self.slot.root.fd.metadata().map_err(failed)
    }

    pub fn file_path(&self) -> &str {
        &self.slot.path
    }

    pub fn file_metadata(&self, path: &str) -> EonResult<Metadata> {
        self.lookup(path)?.metadata().map_err(failed)
    }

    pub fn read_file(&self, path: &str) -> EonResult<Vec<u8>> {
        read_extent(self.lookup(path)?, self.len)
    }

    fn lookup(&self, path: &str) -> EonResult<&File> {
        if path != self.file_path() {
            return Err(invalid("not a file in the selected checkout"));
        }
        // Resolve against this branch, never the scratch parent. O_PATH avoids
        // opening a replacement device/FIFO. Only the pinned export supplies I/O.
        let pin: File = fs::openat2(
            &self.slot.root.fd,
            path,
            OFlags::PATH | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
        )
        .map_err(failed)?
        .into();
        if identity(&pin)? != identity(&self.file.fd)?
            || pin.metadata().map_err(failed)?.nlink() != 1
        {
            return Err(invalid("selected worktree entry changed"));
        }
        Ok(&self.file.fd)
    }
}

#[cfg(test)]
mod compiler_tests {
    use super::super::fixture::{Fixture, write};
    use super::*;

    #[test]
    fn fallback_only_unlinks_its_own_new_empty_file() {
        let f = Fixture::new();
        let scratch = f.root();
        let store = WorktreeStore::from_directory(File::open(&scratch).unwrap()).unwrap();
        let existing = scratch.join("existing");
        write(&existing, b"keep");
        assert!(store.0.create_unlinked("existing", Mode::RUSR).is_err());
        let fd = store.0.create_unlinked("fresh", Mode::RUSR).unwrap();
        assert_eq!(fd.metadata().unwrap().nlink(), 0);
        fd.write_all_at(b"only via fd", 0).unwrap();
        assert_eq!(read_extent(&fd, 11).unwrap(), b"only via fd");
        assert_eq!(std::fs::read_dir(scratch).unwrap().count(), 1);
        assert_eq!(std::fs::read(existing).unwrap(), b"keep");
    }

    #[test]
    fn an_unlinked_read_only_lower_survives_divergent_files_unchanged() {
        let f = Fixture::new();
        let scratch = f.root();
        let store = WorktreeStore::from_directory(File::open(&scratch).unwrap()).unwrap();
        let lower = store.lower(b"base").unwrap();
        assert_eq!(lower.metadata().unwrap().nlink(), 0);
        assert!(lower.write_at(b"x", 0).is_err());
        assert!(lower.set_len(0).is_err());
        let fork = || {
            BranchTree::new(&store, "pkg/Data", lower.clone(), 4)
                .unwrap()
                .0
        };
        let mut a = fork();
        let b = fork();
        a.overwrite(1, b"y").unwrap();
        let upper = a.upper.as_ref().unwrap();
        let m = upper.metadata().unwrap();
        assert!(m.is_file());
        assert_eq!((m.nlink(), m.mode() & 0o777), (0, 0o600));
        assert_ne!(identity(upper).unwrap(), identity(&lower).unwrap());
        assert_eq!(a.freeze().unwrap(), b"byse");
        assert_eq!(b.freeze().unwrap(), b"base");
        assert_eq!(read_extent(&lower, 4).unwrap(), b"base");
        assert_eq!(std::fs::read_dir(scratch).unwrap().count(), 0);
    }
}
