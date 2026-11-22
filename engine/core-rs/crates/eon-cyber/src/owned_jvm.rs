//! Explicitly invoked, fixed HotSpot JDI observation (not a DAP adapter). Only
//! the new owned child and its inspector share the anonymous JDWP pipes. There
//! is no target, PID, command, expression, port, path or raw protocol API.
//! The pinned Linux x64 Temurin 17.0.21+9 subset is copied from the installed
//! package and SHA-256 checked in private staging before executing any of it.
//! No downloads or caller-supplied JVM options. Requires the trusted host OS
//! tools below and an owner-controlled workspace with a `target` directory.
use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
use std::io::{self, Read};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

const JDK: &str = "/usr/lib/jvm/temurin-17-jdk-amd64";
const GCC: &str = "/usr/bin/x86_64-linux-gnu-gcc-13";
const HASHES: &str = include_str!("../fixtures/owned_jvm/profile.sha256");
static RUN: Mutex<()> = Mutex::new(());

unsafe extern "C" {
    fn geteuid() -> u32;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceFrame {
    pub method: &'static str,
    pub line: u32,
}

/// `VirtualMachine.instanceCounts` for `OwnedFixture` and `OwnedFixture$Marker`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HeapCounts {
    pub owned_fixture: u64,
    pub marker: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Observation {
    pub frames: [SourceFrame; 3],
    pub counts: HeapCounts,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Busy,
    Unavailable,
    Build,
    Inspection,
    UnexpectedFixture,
    Deadline,
    Cleanup,
}

type Result<T> = std::result::Result<T, Error>;

struct Workspace(PathBuf);

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

impl Workspace {
    fn create() -> io::Result<Self> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .ok_or(io::ErrorKind::NotFound)?
            .join("target");
        // SAFETY: Linux geteuid has no arguments or preconditions.
        let uid = unsafe { geteuid() };
        for path in root.ancestors() {
            let m = path.symlink_metadata()?;
            if !m.is_dir() || m.mode() & 0o022 != 0 || (m.uid() != 0 && m.uid() != uid) {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
        }
        let mut nonce = [0; 16];
        File::open("/dev/urandom")?.read_exact(&mut nonce)?;
        let path = root.join(format!("owned-jvm-{:032x}", u128::from_ne_bytes(nonce)));
        DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }

    fn stage_profile(&self, dest: &Path) -> io::Result<()> {
        // Some runner images leave their entire JDK writable. Do not execute
        // in place, follow a recursive copy, or allow unchecked plugins/config.
        let mut remaining = 160 * 1024 * 1024;
        for (_, name) in HASHES.lines().filter_map(|line| line.split_once("  ")) {
            let path = dest.join(name);
            fs::create_dir_all(path.parent().ok_or(io::ErrorKind::InvalidInput)?)?;
            let source = OpenOptions::new()
                .read(true)
                .custom_flags(0o400000 | 0o4000) // Linux O_NOFOLLOW | O_NONBLOCK
                .open(Path::new(JDK).join(name))?;
            let meta = source.metadata()?;
            if !meta.is_file() || meta.len() > remaining {
                return Err(io::ErrorKind::InvalidData.into());
            }
            let mut out = File::create_new(&path)?;
            let n = io::copy(&mut source.take(meta.len() + 1), &mut out)?;
            if n != meta.len() {
                return Err(io::ErrorKind::InvalidData.into());
            }
            remaining -= n;
            out.set_permissions(Permissions::from_mode(0o500))?;
        }
        fs::write(self.0.join("profile.sha256"), HASHES)
    }

    fn tool(&self, binary: &Path) -> Command {
        // GNU timeout supervises a dedicated group (includes javac/cc children).
        let mut cmd = Command::new("/usr/bin/timeout");
        cmd.args(["--kill-after=2s", "20s"])
            .arg(binary)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LC_ALL", "C")
            .env("HOME", &self.0)
            .env("TMPDIR", &self.0)
            .current_dir(&self.0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd
    }
}

fn run(cmd: &mut Command, failure: Error) -> Result<()> {
    let status = cmd.status().map_err(|_| Error::Unavailable)?;
    match status.code() {
        Some(0) => Ok(()),
        Some(124 | 137) => Err(Error::Deadline),
        _ => Err(failure),
    }
}

enum Mode {
    Observe,
    #[cfg(test)]
    Cleanup,
}

fn inspect(work: &Workspace, mode: Mode) -> Result<Observation> {
    let dir = &work.0;
    let jdk = dir.join("jdk");
    work.stage_profile(&jdk).map_err(|_| Error::Unavailable)?;
    run(
        work.tool(Path::new("/usr/bin/sha256sum"))
            .current_dir(&jdk)
            .args(["--strict", "--check", "--status"])
            .arg(dir.join("profile.sha256")),
        Error::Unavailable,
    )?;
    for (name, source) in [
        (
            "ownedpipe.c",
            include_str!("../fixtures/owned_jvm/ownedpipe.c"),
        ),
        (
            "OwnedFixture.java",
            include_str!("../fixtures/owned_jvm/OwnedFixture.java"),
        ),
        (
            "Inspect.java",
            include_str!("../fixtures/owned_jvm/Inspect.java"),
        ),
    ] {
        fs::write(dir.join(name), source).map_err(|_| Error::Build)?;
    }
    run(
        work.tool(Path::new(GCC))
            .args([
                "-std=c11",
                "-D_POSIX_C_SOURCE=200809L",
                "-Wall",
                "-Wextra",
                "-Wno-unused-parameter",
                "-Werror",
                "-O2",
                "-fPIC",
                "-shared",
                "-I",
            ])
            .arg(jdk.join("include"))
            .arg("-I")
            .arg(jdk.join("include/linux"))
            .arg(dir.join("ownedpipe.c"))
            .arg("-o")
            .arg(dir.join("libownedpipe.so")),
        Error::Build,
    )?;
    let mut javac = work.tool(&jdk.join("bin/javac"));
    javac
        .args([
            "-J-XX:-UsePerfData",
            "-J-XX:+DisableAttachMechanism",
            "-J-XX:-CreateCoredumpOnCrash",
            "-J-Xshare:off",
            "-J-XX:ActiveProcessorCount=1",
            "-J-Xmx96m",
            "-g:source,lines",
            "-proc:none",
            "--add-modules",
            "jdk.jdi",
            "-d",
        ])
        .arg(dir)
        .arg(format!("-J-Djava.io.tmpdir={}", dir.display()))
        .arg(dir.join("OwnedFixture.java"))
        .arg(dir.join("Inspect.java"));
    let inspector = match mode {
        Mode::Observe => "Inspect",
        #[cfg(test)]
        Mode::Cleanup => {
            let cleanup = dir.join("InspectCleanup.java");
            fs::write(
                &cleanup,
                include_str!("../fixtures/owned_jvm/InspectCleanup.java"),
            )
            .map_err(|_| Error::Build)?;
            javac.arg(cleanup);
            "InspectCleanup"
        }
    };
    run(&mut javac, Error::Build)?;
    let report = dir.join("observation");
    run(
        work.tool(&jdk.join("bin/java"))
            .args([
                "-XX:-UsePerfData",
                "-XX:+DisableAttachMechanism",
                "-XX:-CreateCoredumpOnCrash",
                "-Xshare:off",
                "-XX:ActiveProcessorCount=1",
                "-Xmx64m",
                "--add-modules",
                "jdk.jdi",
                "-cp",
            ])
            .arg(dir)
            .arg(format!("-Djava.io.tmpdir={}", dir.display()))
            .arg(inspector)
            .stdout(File::create_new(&report).map_err(|_| Error::Inspection)?),
        Error::Inspection,
    )?;
    let mut buf = String::new();
    File::open(report)
        .and_then(|f| f.take(97).read_to_string(&mut buf))
        .map_err(|_| Error::UnexpectedFixture)?;
    decode(&buf)
}

fn decode(record: &str) -> Result<Observation> {
    if record.len() > 96 {
        return Err(Error::UnexpectedFixture);
    }
    let mut fields = record
        .strip_suffix('\n')
        .ok_or(Error::UnexpectedFixture)?
        .split(' ');
    let mut frame = |method, expected_line| {
        let (name, number) = fields
            .next()
            .and_then(|f| f.split_once(':'))
            .ok_or(Error::UnexpectedFixture)?;
        let line = number.parse().map_err(|_| Error::UnexpectedFixture)?;
        if name != method || line != expected_line {
            return Err(Error::UnexpectedFixture);
        }
        Ok(SourceFrame { method, line })
    };
    let frames = [
        frame("mark", 7)?,
        frame("selected", 18)?,
        frame("main", 23)?,
    ];
    let mut count = || {
        fields
            .next()
            .and_then(|n| n.parse().ok())
            .ok_or(Error::UnexpectedFixture)
    };
    let counts = HeapCounts {
        owned_fixture: count()?,
        marker: count()?,
    };
    if fields.next().is_some() || counts.owned_fixture != 0 || counts.marker != 3 {
        return Err(Error::UnexpectedFixture);
    }
    Ok(Observation { frames, counts })
}

fn observe(mode: Mode) -> Result<Observation> {
    let _run = RUN.try_lock().map_err(|_| Error::Busy)?;
    let work = Workspace::create().map_err(|_| Error::Unavailable)?;
    let result = inspect(&work, mode);
    let path = work.0.clone();
    drop(work);
    if path.try_exists().map_err(|_| Error::Cleanup)? {
        return Err(Error::Cleanup);
    }
    result
}

/// Stop only at the second visit to `OwnedFixture.mark`, verify the three
/// expected source frames, count the two known classes, and reap the child.
pub fn observe_fixture() -> Result<Observation> {
    observe(Mode::Observe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_other_frames_counts_or_payloads() {
        for bad in [
            "mark:7 first:18 main:23 0 3\n",
            "mark:7 selected:18 main:23 0 4\n",
            "mark:7 selected:18 main:23 0 3\nextra",
        ] {
            assert_eq!(decode(bad), Err(Error::UnexpectedFixture));
        }
    }

    #[test]
    #[ignore = "requires pinned Linux x64 Temurin 17.0.21+9 local profile"]
    fn reaps_suspended_fixture() {
        assert_eq!(observe(Mode::Cleanup).unwrap().counts.marker, 3);
    }
}
