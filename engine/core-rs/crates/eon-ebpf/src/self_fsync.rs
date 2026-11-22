//! Counts entries to the calling process's dynamically linked libc `fsync` on Linux 6.11+.
//! This does not measure direct syscalls, success, bytes, or persistence.

use aya::{
    Ebpf,
    maps::{Array, MapData},
    programs::{UProbe, uprobe::UProbeScope},
};
use linux_raw_sys::general::{PROCFS_IOCTL_MAGIC, procmap_query, procmap_query_flags::*};
use std::{
    ffi::{CStr, OsStr},
    fs::File,
    os::{
        fd::AsRawFd,
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::Path,
};

#[derive(Debug, PartialEq, Eq)]
pub enum SensorError {
    Library,
    Load,
    Map,
    Attach,
    NotOwner,
}

/// Resolve just the mapping covering our linked fsync. Verify and hold its inode
/// through attach: a libc upgrade must not turn a stale binding into observed zero.
fn libc_entry() -> Result<(File, u64), SensorError> {
    let maps = File::open("/proc/self/maps").map_err(|_| SensorError::Library)?;
    let mut name = [0u8; libc::PATH_MAX as usize];
    let addr = libc::fsync as *const () as usize as u64;
    let mut query = procmap_query {
        size: size_of::<procmap_query>() as u64,
        query_addr: addr,
        query_flags: PROCMAP_QUERY_VMA_EXECUTABLE as u64 | PROCMAP_QUERY_FILE_BACKED_VMA as u64,
        vma_name_size: name.len() as u32,
        vma_name_addr: name.as_mut_ptr() as usize as u64,
        // SAFETY: remaining UAPI fields are integers; zero requests no build ID.
        ..unsafe { std::mem::zeroed() }
    };
    // linux/fs.h: PROCMAP_QUERY. No iteration over mappings or other processes.
    let op = libc::_IOWR::<procmap_query>(PROCFS_IOCTL_MAGIC.into(), 17);
    // SAFETY: query and the only requested output buffer remain valid for ioctl.
    if unsafe { libc::ioctl(maps.as_raw_fd(), op, &mut query) } != 0 {
        return Err(SensorError::Library);
    }
    let bytes = name
        .get(..query.vma_name_size as usize)
        .ok_or(SensorError::Library)?;
    let name = CStr::from_bytes_with_nul(bytes).map_err(|_| SensorError::Library)?;
    let path = Path::new(OsStr::from_bytes(name.to_bytes()));
    if !path.is_absolute() || path.file_name() != Some(OsStr::new("libc.so.6")) {
        return Err(SensorError::Library);
    }
    let file = File::options()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| SensorError::Library)?;
    let meta = file.metadata().map_err(|_| SensorError::Library)?;
    if !meta.is_file()
        || meta.ino() != query.inode
        || libc::makedev(query.dev_major, query.dev_minor) != meta.dev()
    {
        return Err(SensorError::Library);
    }
    let offset = addr
        .checked_sub(query.vma_start)
        .and_then(|d| d.checked_add(query.vma_offset))
        .filter(|o| addr < query.vma_end && *o < meta.len())
        .ok_or(SensorError::Library)?;
    Ok((file, offset))
}

pub struct SelfFsyncCounter {
    // Aya owns the non-pinned link; dropping it detaches even on early return.
    _probe: Ebpf,
    calls: Array<MapData, u64>,
    owner: u32,
}

impl SelfFsyncCounter {
    /// Explicit opt-in. The kernel binds the link to the current thread group
    /// by task identity, not a userspace PID comparison after collection.
    pub fn attach() -> Result<Self, SensorError> {
        let (libc, offset) = libc_entry()?;
        let mut ebpf = Ebpf::load(aya::include_bytes_aligned!(concat!(
            env!("OUT_DIR"),
            "/self_fsync.o"
        )))
        .map_err(|_| SensorError::Load)?;
        let calls = Array::try_from(ebpf.take_map("FSYNC_CALLS").ok_or(SensorError::Map)?)
            .map_err(|_| SensorError::Map)?;
        let program: &mut UProbe = ebpf
            .program_mut("self_fsync")
            .ok_or(SensorError::Load)?
            .try_into()
            .map_err(|_| SensorError::Load)?;
        program.load().map_err(|_| SensorError::Load)?;
        // The fixed .multi section requires a native link; never fall back to
        // a system-wide attach. CallingProcess passes a nonzero PID on this ABI.
        program
            .attach(
                [offset],
                format!("/proc/self/fd/{}", libc.as_raw_fd()),
                UProbeScope::CallingProcess,
            )
            .map_err(|_| SensorError::Attach)?;
        Ok(Self {
            _probe: ebpf,
            calls,
            owner: std::process::id(),
        })
    }

    pub fn count(&self) -> Result<u64, SensorError> {
        if self.owner != std::process::id() {
            return Err(SensorError::NotOwner);
        }
        self.calls.get(&0, 0).map_err(|_| SensorError::Map)
    }
}
