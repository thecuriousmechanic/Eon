//! Stock Bionic x86_64. Only the exact static filter from our extracted APK
//! native library directory can run. No toybox, shell, JIT/memfd execution,
//! writable helper directory, or policy/SELinux change.
use std::ffi::{CStr, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Component, Path};

use rustix::fs::OFlags;
use sha2::{Digest, Sha256};

use super::*;
use crate::pty::StreamFilter;

const NAME: &str = "libeon_pty_worker.so";
const MANIFEST: &str = include_str!("../../../provision/android-pty/worker.sha256");

pub(in crate::pty) struct Programs(File);

fn package_library() -> io::Result<PathBuf> {
    // Unlike /proc/self/exe, dladdr also identifies the native module when
    // hosted inside ART. Copy the loader-owned string while this code is live.
    unsafe {
        let mut info = MaybeUninit::<libc::Dl_info>::uninit();
        if libc::dladdr(package_library as *const _, info.as_mut_ptr()) == 0 {
            return Err(io::Error::from(ErrorKind::Unsupported));
        }
        let name = info.assume_init().dli_fname;
        if name.is_null() {
            return Err(io::Error::from(ErrorKind::Unsupported));
        }
        Ok(Path::new(OsStr::from_bytes(CStr::from_ptr(name).to_bytes())).to_owned())
    }
}

impl Programs {
    #[cfg(test)]
    pub(in crate::pty) fn pinned_image(&self) -> &File {
        &self.0
    }

    pub(in crate::pty) fn pin() -> io::Result<Self> {
        let denied = || io::Error::from(ErrorKind::PermissionDenied);
        let source = package_library()?;
        let dir = source.parent().ok_or_else(denied)?;
        if !dir.starts_with("/data/app") || !dir.ends_with("lib/x86_64") {
            return Err(io::Error::new(
                ErrorKind::Unsupported,
                "expected extracted APK library",
            ));
        }

        if fs::fstatfs(File::open("/proc/self/fd")?)?.f_type != fs::PROC_SUPER_MAGIC {
            return Err(denied());
        }
        // O_PATH requires search access, not a listing of /data or /data/app.
        // APK directories may be group-writable by Android's system UID/GID.
        let mut parent: File = fs::open(
            "/",
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?
        .into();
        for part in dir.strip_prefix("/").map_err(|_| denied())?.components() {
            let Component::Normal(name) = part else {
                return Err(denied());
            };
            parent = fs::openat(
                &parent,
                name,
                OFlags::PATH | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::DIRECTORY,
                Mode::empty(),
            )?
            .into();
            let m = parent.metadata()?;
            if !matches!(m.uid(), 0 | 1000)
                || m.mode() & 2 != 0
                || (m.mode() & 0o020 != 0 && !matches!(m.gid(), 0 | 1000))
            {
                return Err(denied());
            }
        }
        // Pin as metadata first: even an invalid installation must not open
        // a special file for I/O. Hash and execute this one regular inode.
        let pin: File = fs::openat(
            &parent,
            NAME,
            OFlags::PATH | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )?
        .into();
        let m = pin.metadata()?;
        if !m.is_file()
            || m.uid() != 1000
            || m.mode() & 0o6022 != 0
            || m.mode() & 0o111 == 0
            || m.len() > 1024 * 1024
        {
            return Err(denied());
        }
        let image = File::options()
            .read(true)
            .custom_flags(libc::O_NOCTTY)
            .open(fd_path(&pin))?;
        if image.as_raw_fd() < 3 {
            return Err(denied());
        }
        let mut bytes = vec![0; m.len() as usize];
        image.read_exact_at(&mut bytes, 0)?;
        if Some(format!("{:x}", Sha256::digest(bytes)).as_str())
            != MANIFEST.split_whitespace().next()
        {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "unexpected Android PTY helper",
            ));
        }
        Ok(Self(image))
    }

    pub(in crate::pty) fn launch(&self, mode: PtyMode) -> io::Result<Terminal> {
        let mut cmd = Command::new(fd_path(&self.0));
        cmd.env_clear()
            .current_dir("/")
            .arg0("eon-pty")
            .arg(match mode {
                PtyMode::Loopback => "copy",
                PtyMode::Filter(StreamFilter::AsciiUppercase) => "upper",
                PtyMode::Filter(StreamFilter::SqueezeSpaces) => "squeeze",
            });
        spawn_terminal(mode, cmd, super::private_locked_pair, |cmd| {
            // SAFETY: only child-side syscalls; retain Command's spawn-error
            // pipe and pinned image through exec, but no inherited fd beyond it.
            unsafe {
                cmd.pre_exec(|| {
                    limit(Resource::Core, 0)?;
                    limit(Resource::As, 64 * 1024 * 1024)?;
                    cvt(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0))?;
                    if libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 1u32 << 2) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            Ok(())
        })
    }
}

#[cfg(test)]
#[test]
#[ignore = "requires signed APK fixture with altered companion, see test-device.sh"]
fn wrong_apk_helper_is_rejected_before_launch() {
    let Err(error) = Programs::pin() else {
        panic!("accepted the wrong companion")
    };
    assert_eq!(error.kind(), ErrorKind::InvalidData);
}
