//! Host-only operations on an operator-selected directory. Requires Linux `openat2`
//! and procfs; there is deliberately no ambient-path fallback. The root handle,
//! not the spelling of its original pathname, is the capability.
//!
//! Edits require a local filesystem, operator-owned directories and files not
//! writable by other principals, and a trusted journal outside the workspace.
//! Cooperating writers lock the parent directory across check, snapshot and rename.
//! Linux rename has no expected-inode argument: this is not a CAS against a
//! privileged or same-UID program that ignores the lock or moves directories.

use std::fmt;
use std::fs::{File, Metadata};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use eon_foundation::group_commit::GroupCommitJournal;
use rustix::fs::{self, AtFlags, Mode, OFlags, ResolveFlags};
use rustix::io::Errno;
use sha2::{Digest, Sha256};

mod recovery;
mod snapshot;
pub use snapshot::SnapshotReceipt;

pub const MAX_READ_BYTES: usize = 1024 * 1024;
pub const MAX_EDIT_BYTES: usize = 1024 * 1024;
const STORE: &str = ".eon-workspace-snapshots";
const BENEATH: ResolveFlags = ResolveFlags::BENEATH
    .union(ResolveFlags::NO_SYMLINKS)
    .union(ResolveFlags::NO_XDEV);
const DIR: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::CLOEXEC);

#[derive(Debug)]
pub enum WorkspaceError {
    Invalid(&'static str),
    Conflict,
    Io(io::Error),
    Journal(eon_foundation::EonError),
    /// Rename succeeded but verification or directory sync failed. Do not retry blindly.
    CommitUncertain {
        journal_seq: u64,
        source: io::Error,
    },
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(msg) => f.write_str(msg),
            Self::Conflict => f.write_str("file changed or another edit holds the directory lock"),
            Self::Io(e) => e.fmt(f),
            Self::Journal(e) => write!(f, "pre-write journal: {e}"),
            Self::CommitUncertain {
                journal_seq,
                source,
            } => {
                write!(
                    f,
                    "edit outcome uncertain after journal sequence {journal_seq}: {source}"
                )
            }
        }
    }
}
impl std::error::Error for WorkspaceError {}
impl From<io::Error> for WorkspaceError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<Errno> for WorkspaceError {
    fn from(e: Errno) -> Self {
        Self::Io(e.into())
    }
}
type Result<T> = std::result::Result<T, WorkspaceError>;

pub struct RangeRead {
    pub bytes: Vec<u8>,
    pub file_len: u64,
    /// Digest of the bytes returned, not of unread portions of the file.
    pub sha256: [u8; 32],
}

#[derive(Debug)]
pub struct EditReceipt {
    pub sha256: [u8; 32],
    /// Private preimage filename beneath `.eon-workspace-snapshots`.
    pub snapshot_id: String,
    pub journal_seq: u64,
}

pub struct Workspace {
    root: File,
}

struct EditLock<'a>(&'a File);

impl<'a> EditLock<'a> {
    fn acquire(parent: &'a File) -> Result<Self> {
        Self::try_lock(parent, fs::FlockOperation::NonBlockingLockExclusive)
    }

    fn try_lock(dir: &'a File, operation: fs::FlockOperation) -> Result<Self> {
        match fs::flock(dir, operation) {
            Ok(()) => Ok(Self(dir)),
            Err(Errno::WOULDBLOCK) => Err(WorkspaceError::Conflict),
            Err(e) => Err(e.into()),
        }
    }
}

impl Drop for EditLock<'_> {
    fn drop(&mut self) {
        // A concurrently forked child can still hold this file description
        // until exec. Closing only our descriptor would leave its flock held.
        let _ = fs::flock(self.0, fs::FlockOperation::Unlock);
    }
}

impl Workspace {
    /// Pin an absolute operator path; even symlinks in its ancestors are rejected.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        if !root.is_absolute() {
            return Err(WorkspaceError::Invalid("workspace root must be absolute"));
        }
        let fd = fs::openat2(fs::CWD, root, DIR, Mode::empty(), ResolveFlags::NO_SYMLINKS)?;
        Ok(Self { root: fd.into() })
    }

    /// Alternatively accept a trusted directory capability rather than a path.
    pub fn from_directory(root: File) -> Result<Self> {
        if !root.metadata()?.is_dir() {
            return Err(WorkspaceError::Invalid("workspace root is not a directory"));
        }
        Ok(Self { root })
    }

    /// A range extending past EOF is clipped. No bytes outside the range are read.
    pub fn read_range(&self, path: &str, offset: u64, length: usize) -> Result<RangeRead> {
        validate(path)?;
        if length > MAX_READ_BYTES || offset.checked_add(length as u64).is_none() {
            return Err(WorkspaceError::Invalid("invalid or oversized byte range"));
        }
        let (mut file, meta) = self.source(path, OFlags::RDONLY)?;
        if offset > meta.len() {
            return Err(WorkspaceError::Invalid("byte range starts beyond EOF"));
        }
        let count = (meta.len() - offset).min(length as u64) as usize;
        let mut bytes = vec![0; count];
        if count != 0 {
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(&mut bytes)?;
        }
        self.unchanged(path, &meta)?;
        Ok(RangeRead {
            sha256: Sha256::digest(&bytes).into(),
            bytes,
            file_len: meta.len(),
        })
    }

    /// Atomically replace one existing regular file. New data is owner-only and
    /// retains only the original owner execute bit. Journal records describe
    /// *preparations*, never committed writes; neither names nor file bytes are
    /// written to the shared journal. Snapshot retention is operator-managed.
    pub fn edit(
        &self,
        path: &str,
        expected_sha256: [u8; 32],
        replacement: &[u8],
        journal: &GroupCommitJournal,
    ) -> Result<EditReceipt> {
        self.edit_with_record(path, expected_sha256, replacement, |record| {
            journal.append_durable(record)
        })
    }

    fn edit_with_record(
        &self,
        path: &str,
        expected_sha256: [u8; 32],
        replacement: &[u8],
        append: impl FnOnce(Vec<u8>) -> eon_foundation::EonResult<u64>,
    ) -> Result<EditReceipt> {
        validate(path)?;
        if replacement.len() > MAX_EDIT_BYTES {
            return Err(WorkspaceError::Invalid("replacement exceeds edit limit"));
        }
        let (parent_path, leaf) = path.rsplit_once('/').unwrap_or((".", path));
        let parent = self.edit_parent(parent_path)?;
        let _lock = EditLock::acquire(&parent)?;

        // RDWR checks actual write authorization without truncating or writing the inode.
        let (file, before) = self.source(path, OFlags::RDWR)?;
        owned_unshared(&before)?;
        if before.mode() & 0o7600 != 0o0600 {
            return Err(WorkspaceError::Invalid("not an editable workspace file"));
        }
        let preimage = read_edit_bytes(&file, &before)?;
        if digest(&preimage) != expected_sha256 {
            return Err(WorkspaceError::Conflict);
        }
        self.unchanged(path, &before)?;

        let store = self.snapshot_directory()?;
        // Different parent directories can still be edited concurrently. A
        // whole-workspace snapshot takes the exclusive side on this same inode.
        let _snapshot_gate = EditLock::try_lock(&store, fs::FlockOperation::NonBlockingLockShared)?;
        let mut snapshot = PrivateFile::create(&store, "preimage")?;
        let snapshot_version = snapshot.persist_bytes(&preimage, Mode::RUSR)?;
        store.sync_all()?;
        let new_sha256 = digest(replacement);
        let record = preparation(
            &self.root.metadata()?,
            path,
            &hex_digest(expected_sha256),
            new_sha256,
            &snapshot.name,
        );
        // A failed journal sync may still have written a frame. Keep its preimage.
        snapshot.remove_on_drop = false;
        let journal_seq =
            append(record.to_string().into_bytes()).map_err(WorkspaceError::Journal)?;

        // Do not put even private replacement bytes on disk ahead of the
        // durable preparation. Recheck the store before creating a new name.
        self.check_snapshot_directory(&store)?;
        let mut stage = PrivateFile::create(&store, "stage")?;
        let owner_mode = Mode::from_bits_truncate(before.mode() & 0o700);
        let stage_version = stage.persist_bytes(replacement, owner_mode)?;
        if !same_inode(
            &parent.metadata()?,
            &self.edit_parent(parent_path)?.metadata()?,
        ) {
            return Err(WorkspaceError::Conflict);
        }
        self.check_snapshot_directory(&store)?;
        // The journal preparation may have taken time. Revalidate both private names,
        // in addition to the public pathname, before consuming either.
        snapshot.check_name(&snapshot_version)?;
        self.unchanged(path, &before)?;
        stage.install(&stage_version, &parent, leaf)?;
        let mut verify = || -> io::Result<()> {
            parent.sync_all()?;
            store.sync_all()?;
            let installed = beneath(&self.root, path, OFlags::PATH)?;
            if !same_inode(&stage.file.metadata()?, &installed.metadata()?) {
                return Err(io::Error::other(
                    "installed file no longer at requested path",
                ));
            }
            stage.file.rewind()?;
            let mut actual = Vec::new();
            (&stage.file)
                .take(MAX_EDIT_BYTES as u64 + 1)
                .read_to_end(&mut actual)?;
            if digest(&actual) != new_sha256 {
                return Err(io::Error::other("installed file digest mismatch"));
            }
            Ok(())
        };
        verify().map_err(|source| WorkspaceError::CommitUncertain {
            journal_seq,
            source,
        })?;
        Ok(EditReceipt {
            sha256: new_sha256,
            snapshot_id: snapshot.name.clone(),
            journal_seq,
        })
    }

    fn edit_parent(&self, path: &str) -> Result<File> {
        // Get a fresh file description even for "." so independent callers conflict on flock.
        let mut dir = beneath(&self.root, ".", DIR)?;
        owned_unshared(&dir.metadata()?)?;
        if path != "." {
            for component in path.split('/') {
                dir = beneath(&dir, component, DIR)?;
                owned_unshared(&dir.metadata()?)?;
            }
        }
        Ok(dir)
    }

    fn snapshot_directory(&self) -> Result<File> {
        match fs::mkdirat(&self.root, STORE, Mode::RWXU) {
            Err(Errno::EXIST) => (),
            result => result?,
        }
        let dir = self.open_snapshot_directory()?;
        self.root.sync_all()?;
        Ok(dir)
    }

    fn open_snapshot_directory(&self) -> Result<File> {
        let dir = beneath(&self.root, STORE, DIR)?;
        let meta = dir.metadata()?;
        owned_unshared(&meta)?;
        if meta.mode() & 0o777 != 0o700 {
            return Err(WorkspaceError::Invalid(
                "snapshot directory must be private",
            ));
        }
        Ok(dir)
    }

    fn check_snapshot_directory(&self, store: &File) -> Result<()> {
        if same_inode(
            &store.metadata()?,
            &self.open_snapshot_directory()?.metadata()?,
        ) {
            Ok(())
        } else {
            Err(WorkspaceError::Conflict)
        }
    }

    fn source(&self, path: &str, access: OFlags) -> Result<(File, Metadata)> {
        open_regular_at(&self.root, path, access, |_| self.reject_store_alias(path))
    }

    fn unchanged(&self, path: &str, original: &Metadata) -> Result<()> {
        self.reject_store_alias(path)?;
        if !same_version(
            original,
            &beneath(&self.root, path, OFlags::PATH)?.metadata()?,
        ) {
            return Err(WorkspaceError::Conflict);
        }
        Ok(())
    }

    fn reject_store_alias(&self, path: &str) -> Result<()> {
        // A case-folding filesystem may accept more than one spelling of STORE.
        // With symlinks, mount crossings, and '..' already excluded, only the
        // first directory can be the private store.
        if let Some((first, _)) = path.split_once('/') {
            let store = match beneath(&self.root, STORE, OFlags::PATH) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
                result => result?,
            };
            if same_inode(
                &store.metadata()?,
                &beneath(&self.root, first, OFlags::PATH)?.metadata()?,
            ) {
                return Err(WorkspaceError::Invalid("reserved workspace directory"));
            }
        }
        Ok(())
    }
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn hex_digest(sha256: [u8; 32]) -> String {
    format!("{:x}", sha2::digest::Output::<Sha256>::from(sha256))
}

fn preparation(
    root: &Metadata,
    path: &str,
    before_hex: &str,
    after: [u8; 32],
    snapshot_id: &str,
) -> serde_json::Value {
    serde_json::json!({
        "type": "workspace.edit.prepared.v1",
        "root_dev": root.dev(),
        "root_ino": root.ino(),
        "path_sha256": format!("{:x}", Sha256::digest(path.as_bytes())),
        "before_sha256": before_hex,
        "after_sha256": hex_digest(after),
        "snapshot_id": snapshot_id,
    })
}

fn read_edit_bytes(file: &File, meta: &Metadata) -> Result<Vec<u8>> {
    if meta.len() > MAX_EDIT_BYTES as u64 {
        return Err(WorkspaceError::Invalid("file exceeds edit limit"));
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.take(MAX_EDIT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 != meta.len() {
        return Err(WorkspaceError::Conflict);
    }
    Ok(bytes)
}

// O_PATH doesn't invoke special-file open handlers. After checking fstat,
// reopen only our live regular-file descriptor via kernel-owned procfs.
fn open_regular_at(
    dir: &File,
    path: &str,
    access: OFlags,
    check: impl FnOnce(&Metadata) -> Result<()>,
) -> Result<(File, Metadata)> {
    let pin = beneath(dir, path, OFlags::PATH)?;
    let meta = pin.metadata()?;
    if !meta.is_file() || meta.nlink() != 1 {
        return Err(WorkspaceError::Invalid(
            "source must be a singly linked regular file",
        ));
    }
    check(&meta)?;
    let procfd = fs::open("/proc/self/fd", DIR, Mode::empty())?;
    if fs::fstatfs(&procfd)?.f_type != fs::PROC_SUPER_MAGIC {
        return Err(WorkspaceError::Invalid("procfs required for safe reopen"));
    }
    let file: File = fs::openat(
        procfd,
        pin.as_raw_fd().to_string(),
        access | OFlags::CLOEXEC,
        Mode::empty(),
    )?
    .into();
    if !same_version(&meta, &file.metadata()?) {
        return Err(WorkspaceError::Conflict);
    }
    Ok((file, meta))
}

fn beneath(dir: &File, path: &str, flags: OFlags) -> io::Result<File> {
    Ok(fs::openat2(dir, path, flags | OFlags::CLOEXEC, Mode::empty(), BENEATH)?.into())
}

fn validate(path: &str) -> Result<()> {
    if path.len() > 4095
        || path.contains(['\\', ':'])
        || path.bytes().any(|b| b.is_ascii_control())
        || path
            .split('/')
            .any(|p| matches!(p, "" | "." | ".." | STORE) || p.len() > 255)
    {
        return Err(WorkspaceError::Invalid(
            "invalid or reserved relative workspace path",
        ));
    }
    Ok(())
}

fn owned_unshared(meta: &Metadata) -> Result<()> {
    if meta.uid() != rustix::process::geteuid().as_raw() || meta.mode() & 0o022 != 0 {
        return Err(WorkspaceError::Invalid(
            "edit requires operator ownership and no shared write access",
        ));
    }
    Ok(())
}

fn same_inode(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}
fn same_version(a: &Metadata, b: &Metadata) -> bool {
    same_inode(a, b)
        && a.nlink() == b.nlink()
        && a.len() == b.len()
        && (a.ctime(), a.ctime_nsec()) == (b.ctime(), b.ctime_nsec())
        && (a.mtime(), a.mtime_nsec()) == (b.mtime(), b.mtime_nsec())
}

struct PrivateFile<'a> {
    dir: &'a File,
    name: String,
    file: File,
    remove_on_drop: bool,
}

impl<'a> PrivateFile<'a> {
    fn create(dir: &'a File, prefix: &str) -> Result<Self> {
        let mut id = [0; 16];
        getrandom::fill(&mut id).map_err(|_| io::Error::other("snapshot entropy unavailable"))?;
        let name = format!("{prefix}-{:032x}", u128::from_le_bytes(id));
        let file = fs::openat(
            dir,
            name.as_str(),
            OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::RDWR | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?
        .into();
        Ok(Self {
            dir,
            name,
            file,
            remove_on_drop: true,
        })
    }

    fn persist_bytes(&mut self, data: &[u8], mode: Mode) -> io::Result<Metadata> {
        self.file.write_all(data)?;
        fs::fchmod(&self.file, mode)?;
        self.file.sync_all()?;
        self.file.metadata()
    }

    fn check_name(&self, version: &Metadata) -> Result<()> {
        let current = beneath(self.dir, &self.name, OFlags::PATH)?.metadata()?;
        if current.nlink() != 1 || !same_version(version, &current) {
            return Err(WorkspaceError::Conflict);
        }
        Ok(())
    }

    fn install(&mut self, version: &Metadata, parent: &File, leaf: &str) -> Result<()> {
        self.check_name(version)?;
        fs::renameat(self.dir, self.name.as_str(), parent, leaf)?;
        self.remove_on_drop = false;
        Ok(())
    }
}

impl Drop for PrivateFile<'_> {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = fs::unlinkat(self.dir, self.name.as_str(), AtFlags::empty());
        }
    }
}

#[cfg(test)]
mod workspace_tests;
