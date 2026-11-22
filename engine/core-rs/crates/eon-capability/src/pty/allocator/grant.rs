//! The installation itself is the operator grant, scoped to *this* mount
//! namespace, devpts superblock and unprivileged identity. Never take an image,
//! PTY path, digest, or mount options on trust from the tool caller.
use std::fs::{self, File};
use std::io::{self, ErrorKind};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::PathBuf;

use rustix::fs::{AtFlags, Mode, OFlags, StatxFlags, fstatfs, major, minor, open, statx};
use rustix::process::{getegid, geteuid};

use super::host::{host_elf, host_file};
use sha2::{Digest, Sha256};

pub(super) const FRAME: &[u8] = b"EON-PTY\x01";

pub(super) fn denied() -> io::Error {
    ErrorKind::PermissionDenied.into()
}

fn private_mount(info: &str, mount: u64, dev: u64) -> bool {
    let device = format!("{}:{}", major(dev), minor(dev));
    let mount = mount.to_string();
    info.lines().any(|line| {
        let Some((before, after)) = line.split_once(" - ") else {
            return false;
        };
        let mut fields = before.split(' ');
        // No shared, master, propagate_from, or other optional fields.
        if fields.next() != Some(&mount)
            || fields.nth(1) != Some(&device)
            || fields.next() != Some("/")
            || fields.next() != Some("/dev/pts")
            || fields.next().is_none()
            || fields.next().is_some()
        {
            return false;
        }
        let mut fs = after.split(' ');
        fs.next() == Some("devpts")
            && fs.nth(1).is_some_and(|opts| {
                let mut opts = opts.split(',');
                opts.clone().any(|o| o == "mode=600")
                    && !opts.any(|o| o.starts_with("uid=") || o.starts_with("gid="))
            })
    })
}

pub(super) struct Grant {
    pub image: File,
    pub ptmx: File,
}

impl Grant {
    pub fn pin() -> io::Result<Self> {
        let uid = geteuid().as_raw();
        let gid = getegid().as_raw();
        let status = fs::read_to_string("/proc/thread-self/status")?;
        let field = |name: &str| {
            status
                .lines()
                .find_map(|s| s.strip_prefix(name))
                .map(str::trim)
        };
        if uid == 0
            || gid == 0
            || field("Uid:") != Some(&format!("{uid}\t{uid}\t{uid}\t{uid}"))
            || field("Gid:") != Some(&format!("{gid}\t{gid}\t{gid}\t{gid}"))
            || field("Groups:") != Some("")
            || ["CapInh:", "CapPrm:", "CapEff:", "CapAmb:"]
                .iter()
                .any(|k| field(k) != Some("0000000000000000"))
        {
            return Err(denied());
        }
        let ptmx: File = open(
            "/dev/pts/ptmx",
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?
        .into();
        let m = ptmx.metadata()?;
        let upstream = fs::metadata("/dev/ptmx")?;
        let sx = statx(&ptmx, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID)?;
        if sx.stx_mask & StatxFlags::MNT_ID.bits() == 0
            || fstatfs(&ptmx)?.f_type != 0x1cd1
            || !m.file_type().is_char_device()
            || (major(m.rdev()), minor(m.rdev())) != (5, 2)
            || (upstream.dev(), upstream.ino()) != (m.dev(), m.ino())
            || !private_mount(
                &fs::read_to_string("/proc/thread-self/mountinfo")?,
                sx.stx_mnt_id,
                m.dev(),
            )
        {
            return Err(denied());
        }
        let namespace = fs::metadata("/proc/thread-self/ns/mnt")?.ino();
        let dir = PathBuf::from(format!(
            "/usr/local/libexec/eon-pty/{namespace}-{}-{uid}-{gid}",
            m.dev()
        ));
        let mut image = host_elf(&dir.join("allocator"))?;
        let mut digest = host_file(&dir.join("allocator.sha256"))?;
        let metadata = image.metadata()?;
        if metadata.mode() & 0o111 == 0
            || metadata.len() > 32 * 1024 * 1024
            || digest.metadata()?.len() != 65
        {
            return Err(denied());
        }
        let mut hash = Sha256::new();
        io::copy(&mut image, &mut hash)?;
        let mut expected = [0; 65];
        io::Read::read_exact(&mut digest, &mut expected)?;
        if expected.as_slice() != format!("{:x}\n", hash.finalize()).as_bytes() {
            return Err(denied());
        }
        Ok(Self { image, ptmx })
    }
}
