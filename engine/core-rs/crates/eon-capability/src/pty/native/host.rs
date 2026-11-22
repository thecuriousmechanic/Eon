use std::fs::File;
use std::io::{self, ErrorKind};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Component, Path};

use rustix::fs::{self, Mode, OFlags};

// Walk from a trusted root using directory handles, never following symlinks.
// Checking the file alone would also accept a link to a different root-owned binary.
pub(super) fn host_file(path: &Path) -> io::Result<File> {
    let mut parts = path.components();
    if parts.next() != Some(Component::RootDir) {
        return Err(io::Error::from(ErrorKind::InvalidInput));
    }
    let mut file = File::open("/")?;
    for part in parts {
        let Component::Normal(name) = part else {
            return Err(io::Error::from(ErrorKind::InvalidInput));
        };
        let m = file.metadata()?;
        if !m.is_dir() || m.uid() != 0 || m.mode() & 0o022 != 0 {
            return Err(io::Error::from(ErrorKind::PermissionDenied));
        }
        file = fs::openat(
            &file,
            name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        )?
        .into();
    }
    let m = file.metadata()?;
    if !m.is_file() || m.uid() != 0 || m.mode() & 0o6022 != 0 || file.as_raw_fd() < 3 {
        return Err(io::Error::from(ErrorKind::PermissionDenied));
    }
    Ok(file)
}

pub(super) fn host_elf(path: &Path) -> io::Result<File> {
    let file = host_file(path)?;
    let mut magic = [0; 4];
    file.read_exact_at(&mut magic, 0)?;
    if &magic != b"\x7fELF" {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "expected host ELF image",
        ));
    }
    Ok(file)
}
