//! Linux storage for caller-authorized captures. Use [`LiveSharedVisualMemory`] during
//! capture and [`SharedVisualMemory`] to replay timestamps offline.
//!
//! Each sealed descriptor contains one JPEG, not the history.
//! This limits what is **in that file**, not what its recipient
//! can access through ambient permissions (for example, same-UID procfs access).
//! Seals protect integrity; they do not sandbox a reader. Nothing here captures a
//! screen, authorizes a reader, or transmits a descriptor.

mod live;
pub use live::LiveSharedVisualMemory;

use std::fs::File;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::FileExt;

use rustix::fs::{MemfdFlags, SealFlags, fcntl_add_seals, memfd_create};

use crate::keyframes::{KeyframeHistory, Luma};
use crate::{EonError, EonResult};

/// The descriptor can be passed explicitly or mapped read-only. Use positional reads or
/// mmap: duplicated descriptors share a seek position. Dropping the ring's reference does
/// not revoke an already delivered descriptor or erase a recipient's copy.
#[derive(Debug)]
pub struct SharedJpeg(File);

#[cfg(any(
    feature = "paddle-ocr",
    feature = "omniparser-icons",
    feature = "qwen-vl"
))]
impl SharedJpeg {
    pub(crate) fn read_limited(&self, limit: usize) -> EonResult<Vec<u8>> {
        let failed = |e: std::io::Error| EonError::OperationFailed {
            operation: "read selected keyframe".into(),
            reason: e.to_string(),
        };
        let size = self.0.metadata().map_err(failed)?.len();
        if size > limit as u64 {
            return Err(EonError::ResourceExhausted {
                resource: "selected JPEG bytes".into(),
                requested: size,
                available: limit as u64,
            });
        }
        // The size is sealed. Positional I/O leaves the producer's and other
        // recipients' shared seek position untouched.
        let mut bytes = vec![0; size as usize];
        self.0.read_exact_at(&mut bytes, 0).map_err(failed)?;
        Ok(bytes)
    }
}

impl AsFd for SharedJpeg {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

fn store(data: &[u8]) -> EonResult<SharedJpeg> {
    let failed = |e: std::io::Error| EonError::OperationFailed {
        operation: "store shared keyframe".into(),
        reason: e.to_string(),
    };
    let fd = memfd_create(
        c"eon-keyframe",
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
    )
    .map_err(|e| failed(e.into()))?;
    let file = File::from(fd);
    file.write_all_at(data, 0).map_err(failed)?;
    fcntl_add_seals(
        &file,
        SealFlags::WRITE | SealFlags::GROW | SealFlags::SHRINK | SealFlags::SEAL,
    )
    .map_err(|e| failed(e.into()))?;
    Ok(SharedJpeg(file))
}

/// Timestamp-driven history backed by one sealed memory file per sampled frame.
/// For offline use: like [`KeyframeHistory`], it does not expire files while idle.
/// Use [`LiveSharedVisualMemory`] for a running capture.
pub type SharedVisualMemory = KeyframeHistory<SharedJpeg>;

impl SharedVisualMemory {
    pub fn observe(
        &mut self,
        now_ms: u64,
        dimensions: (u32, u32),
        thumbnail: &Luma,
        jpeg: &[u8],
    ) -> EonResult<Option<u64>> {
        self.observe_with(now_ms, dimensions, thumbnail, jpeg, store)
    }
}
