use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use eon_kernel::TaskId;
use rustix::fs::{AtFlags, Mode, OFlags, mkdirat, openat, unlinkat};

// Pin Cargo's project-local build directory before creating private children;
// CI checkout ancestry can be shared. Unit tests have no CARGO_TARGET_TMPDIR.
pub(super) struct Fixture {
    parent: File,
    name: String,
    pub(super) dir: File,
}

impl Fixture {
    pub(super) fn new() -> Self {
        let exe = std::env::current_exe().unwrap();
        let scratch = option_env!("CARGO_TARGET_TMPDIR")
            .map(Path::new)
            .unwrap_or_else(|| exe.parent().unwrap());
        let parent = File::open(scratch).unwrap();
        let name = TaskId::new().to_string();
        mkdirat(&parent, name.as_str(), Mode::RWXU).unwrap();
        let dir = File::from(
            openat(
                &parent,
                name.as_str(),
                OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::PATH | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .unwrap(),
        );
        let m = dir.metadata().unwrap();
        assert_eq!(m.uid(), rustix::process::geteuid().as_raw());
        assert_eq!(m.mode() & 0o777, 0o700);
        let fixture = Self { parent, name, dir };
        mkdir(&fixture.path());
        mkdir(&fixture.root());
        fixture
    }

    pub(super) fn path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}/fixture", self.dir.as_raw_fd()))
    }

    pub(super) fn root(&self) -> PathBuf {
        self.path().join("workspace")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(self.path()).unwrap();
        unlinkat(&self.parent, self.name.as_str(), AtFlags::REMOVEDIR).unwrap();
    }
}

pub(super) fn mkdir(path: &Path) {
    DirBuilder::new().mode(0o700).create(path).unwrap();
}

pub(super) fn write(path: &Path, bytes: &[u8]) {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}
