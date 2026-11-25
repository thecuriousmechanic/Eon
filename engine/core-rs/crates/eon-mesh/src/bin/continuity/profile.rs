use rustix::{
    fs::{Mode, OFlags, open, openat},
    process::geteuid,
};
use std::{
    fs::{File, Metadata},
    io::Read,
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    path::{Component, Path},
};

const PIN: OFlags = OFlags::PATH.union(OFlags::NOFOLLOW).union(OFlags::CLOEXEC);
const DENIED: &str = "unavailable_or_untrusted_profile";

fn trusted(meta: &Metadata) -> bool {
    (meta.uid() == 0 || meta.uid() == geteuid().as_raw()) && meta.mode() & 0o022 == 0
}

pub(super) struct Profile(File);

impl Profile {
    pub(super) fn open(path: &Path) -> Result<Self, &'static str> {
        let relative = path
            .strip_prefix("/")
            .map_err(|_| "expected_absolute_profile_path")?;
        let root = open("/", PIN | OFlags::DIRECTORY, Mode::empty()).map_err(|_| DENIED)?;
        Self::walk(root.into(), relative)
    }

    fn walk(mut dir: File, relative: &Path) -> Result<Self, &'static str> {
        let flags = PIN | OFlags::DIRECTORY;
        for part in relative.components() {
            let Component::Normal(name) = part else {
                return Err(DENIED);
            };
            if !trusted(&dir.metadata().map_err(|_| DENIED)?) {
                return Err(DENIED);
            }
            dir = openat(&dir, name, flags, Mode::empty())
                .map_err(|_| DENIED)?
                .into();
        }
        if !trusted(&dir.metadata().map_err(|_| DENIED)?) {
            return Err(DENIED);
        }
        Ok(Self(dir))
    }

    pub(super) fn read(&self, name: &str, max: u64) -> Result<Vec<u8>, &'static str> {
        // Only fixed single-component names are supplied by the CLI. O_PATH
        // acquires an inode without opening a device or waiting for a FIFO.
        let pin: File = openat(&self.0, name, PIN, Mode::empty())
            .map_err(|_| DENIED)?
            .into();
        let meta = pin.metadata().map_err(|_| DENIED)?;
        if !meta.is_file()
            || !trusted(&meta)
            || meta.nlink() != 1
            || (name == "identity.key" && meta.mode() & 0o077 != 0)
        {
            return Err(DENIED);
        }
        let mut bytes = Vec::new();
        File::open(format!("/proc/self/fd/{}", pin.as_raw_fd()))
            .map_err(|_| DENIED)?
            .take(max + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| DENIED)?;
        if bytes.is_empty() || bytes.len() as u64 > max {
            return Err("invalid_profile_file_size");
        }
        Ok(bytes)
    }

    pub(super) fn parse<T: std::str::FromStr>(&self, name: &str) -> Result<T, &'static str> {
        let bytes = self.read(name, 128)?;
        std::str::from_utf8(&bytes)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .ok_or("invalid_profile_value")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::{DirBuilderExt, PermissionsExt, symlink},
    };

    #[test]
    fn profile_pins_directory_and_rejects_links_fifo_and_exposed_key() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("target/profile-test-{}", std::process::id()));
        fs::create_dir_all(base.parent().unwrap()).unwrap();
        fs::DirBuilder::new().mode(0o700).create(&base).unwrap();
        let shared = base.join("shared-ci");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o770)).unwrap();
        let private = shared.join("operator");
        fs::DirBuilder::new().mode(0o700).create(&private).unwrap();
        let path = private.join("profile");
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        assert!(Profile::open(&path).is_err()); // CLI must reject the shared ancestor
        // Test only the subtree we own. The CLI always starts its walk at /.
        let root = File::open(&private).unwrap();
        let name = Path::new("profile");
        let profile = Profile::walk(root.try_clone().unwrap(), name).unwrap();
        let moved = base.join("moved");
        fs::rename(&path, &moved).unwrap();
        symlink(&base, &path).unwrap(); // points away from the pinned directory
        assert!(Profile::walk(root, name).is_err());
        let key = moved.join("identity.key");
        fs::write(&key, b"not a key").unwrap();
        fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(profile.read("identity.key", 128).is_err());
        fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(profile.read("identity.key", 128).unwrap(), b"not a key");
        symlink("identity.key", moved.join("peer.pin")).unwrap();
        assert!(profile.read("peer.pin", 128).is_err());
        rustix::fs::mkfifoat(&profile.0, "ca.der", Mode::RUSR).unwrap();
        assert!(profile.read("ca.der", 1024).is_err()); // no writer on FIFO
        fs::remove_dir_all(base).unwrap();
    }
}
