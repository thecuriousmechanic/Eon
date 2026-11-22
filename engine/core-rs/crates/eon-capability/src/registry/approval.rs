use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::sync::Arc;

use eon_primitives::{EonError, EonResult};

use super::AllowedTool;

fn storage(e: impl std::fmt::Display) -> EonError {
    EonError::OperationFailed {
        operation: "tool.durable_approval".into(),
        reason: e.to_string(),
    }
}

fn denied() -> EonError {
    EonError::SecurityViolation("durable tool approval missing, changed or quarantined".into())
}

/// Private durable state provisioned by the trusted host outside every tool
/// workspace. All registries must reopen the same stable, host-controlled
/// directory on a filesystem with working locking and fsync semantics.
/// No file is created by registration, dispatch, or opening this store.
#[derive(Clone)]
pub struct DurableApprovals(Arc<File>);

impl DurableApprovals {
    pub fn open(directory: &Path) -> EonResult<Self> {
        let dir = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(directory)
            .map_err(storage)?;
        let meta = dir.metadata().map_err(storage)?;
        // SAFETY: geteuid has no arguments or failure mode.
        if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
            return Err(EonError::SecurityViolation(
                "approval directory must be private and host-owned".into(),
            ));
        }
        Ok(Self(Arc::new(dir)))
    }

    fn locked_file(&self, name: &str, create: bool) -> EonResult<Record> {
        let key =
            CString::new(blake3::hash(name.as_bytes()).to_hex().as_str()).expect("hex has no NUL");
        let flags = libc::O_RDWR
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if create { libc::O_CREAT } else { 0 };
        // SAFETY: pinned directory descriptor, single generated component and mode
        // when O_CREAT is present. Ownership of a successful fd passes to File.
        let fd = unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                key.as_ptr(),
                flags,
                0o600 as libc::mode_t,
            )
        };
        if fd < 0 {
            let err = std::io::Error::last_os_error();
            return Err(if err.kind() == std::io::ErrorKind::NotFound {
                denied()
            } else {
                storage(err)
            });
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let meta = file.metadata().map_err(storage)?;
        if !meta.is_file()
            || meta.nlink() != 1
            || meta.mode() & 0o077 != 0
            || meta.uid() != self.0.metadata().map_err(storage)?.uid()
        {
            return Err(denied());
        }
        file.try_lock().map_err(storage)?;
        Ok(Record(file))
    }

    /// Explicit operator action only, never on a listing or as startup recovery.
    /// Approves exactly one reviewed metadata + code identity. Rebuild a registry
    /// that already latched a local quarantine after re-approval.
    pub fn approve(&self, entry: &AllowedTool) -> EonResult<()> {
        let record = self.locked_file(&entry.descriptor.name, true)?;
        record.consume()?;
        // Make the directory entry durable while it still denies all versions.
        self.0.sync_all().map_err(storage)?;
        record.restore(&entry.identity())
    }

    pub(super) fn acquire(&self, name: &str, identity: &[u8; 32]) -> EonResult<Record> {
        let record = self.locked_file(name, false)?;
        let mut bytes = [0; 32];
        if record.0.metadata().map_err(storage)?.len() != bytes.len() as u64 {
            return Err(denied());
        }
        record.0.read_exact_at(&mut bytes, 0).map_err(storage)?;
        if &bytes != identity {
            return Err(denied());
        }
        Ok(record)
    }
}

pub(super) struct Record(File);

impl Drop for Record {
    fn drop(&mut self) {
        // A concurrent fork may briefly hold a duplicate until exec. Release
        // our lock at the end of the critical section, not at the last close.
        let _ = self.0.unlock();
    }
}

impl Record {
    /// Persist the default-deny state *before* inspecting a new listing.
    /// A crash or error during verification cannot clear an observed quarantine.
    pub(super) fn consume(&self) -> EonResult<()> {
        self.0
            .set_len(0)
            .and_then(|()| self.0.sync_all())
            .map_err(storage)
    }

    pub(super) fn restore(&self, identity: &[u8; 32]) -> EonResult<()> {
        if let Err(error) = self
            .0
            .write_all_at(identity, 0)
            .and_then(|()| self.0.sync_all())
        {
            self.consume()?;
            return Err(storage(error));
        }
        Ok(())
    }
}
