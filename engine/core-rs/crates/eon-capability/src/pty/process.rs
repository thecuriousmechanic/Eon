//! One-shot `process.exec`. Only the closed argv table and supplied stdin reach
//! the child. No command strings, file operands, environment or credential sink.
use std::fs::File;
use std::io::{self, ErrorKind, Read, Seek, Write};
use std::process::{Child, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use eon_foundation::identity::{Identity, IdentityAuthority};
use rustix::fs::{MemfdFlags, Mode, SealFlags, fchmod, fcntl_add_seals, memfd_create};

use super::native::{Image, Utility};
use super::{StreamFilter, authorize};

pub const MAX_PROCESS_INPUT: usize = 64 * 1024;
/// Maximum bytes accepted from each of stdout and stderr independently.
pub const MAX_PROCESS_OUTPUT: usize = 32 * 1024;
pub const PROCESS_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug)]
pub enum ProcessMode {
    Filter(StreamFilter),
    CountBytes,
    /// Counts newline bytes, not a trailing partial line.
    CountLines,
}

pub struct ProcessOutput {
    pub status: ExitStatus,
    /// Opaque, untrusted bytes; render inertly, not directly to a terminal.
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub struct ProcessProvider {
    authority: IdentityAuthority,
    tr: Image,
    wc: Image,
}

impl ProcessProvider {
    pub fn new(authority: &IdentityAuthority) -> io::Result<Self> {
        Ok(Self {
            authority: authority.clone(),
            tr: Image::pin(Utility::Tr)?,
            wc: Image::pin(Utility::Wc)?,
        })
    }

    /// One child at a time per provider. The deadline starts before preparing
    /// stdin and launch; synchronous OS setup and kill/reap are not preemptible.
    pub fn exec(
        &mut self,
        caller: &Identity,
        mode: ProcessMode,
        input: &[u8],
    ) -> io::Result<ProcessOutput> {
        authorize(&self.authority, caller)?;
        if input.len() > MAX_PROCESS_INPUT {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "process input limit",
            ));
        }
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        let mut stdin = anonymous_file()?;
        stdin.write_all(input)?;
        stdin.rewind()?;
        fcntl_add_seals(
            &stdin,
            SealFlags::WRITE | SealFlags::GROW | SealFlags::SHRINK | SealFlags::SEAL,
        )?;
        let (image, args): (_, &[&str]) = match mode {
            ProcessMode::Filter(filter) => (&self.tr, filter.args()),
            ProcessMode::CountBytes => (&self.wc, &["-c"]),
            ProcessMode::CountLines => (&self.wc, &["-l"]),
        };
        Running::spawn(image, args, stdin.into())?.finish(deadline)
    }
}

fn anonymous_file() -> io::Result<File> {
    let fd = memfd_create(
        c"eon-filter",
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
    )?;
    // Data descriptors only; the worker's syscall filter also denies chmod.
    fchmod(&fd, Mode::RUSR | Mode::WUSR)?;
    Ok(fd.into())
}

struct Running {
    child: Child,
    stdout: File,
    stderr: File,
}

impl Running {
    fn spawn(image: &Image, args: &[&'static str], stdin: Stdio) -> io::Result<Self> {
        // Bounded anonymous files avoid pipe deadlock and helper threads. The
        // extra byte distinguishes an exact fit from overflow without returning
        // partial output. RLIMIT_FSIZE applies before exec, not after buffering.
        let stdout = anonymous_file()?;
        let stderr = anonymous_file()?;
        let child = image
            .batch(args, MAX_PROCESS_OUTPUT as u64 + 1)?
            .stdin(stdin)
            .stdout(stdout.try_clone()?)
            .stderr(stderr.try_clone()?)
            .spawn()?;
        Ok(Self {
            child,
            stdout,
            stderr,
        })
    }

    fn finish(mut self, deadline: Instant) -> io::Result<ProcessOutput> {
        let status = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(ErrorKind::TimedOut.into());
            }
            if let Some(status) = self.child.try_wait()? {
                break status;
            }
            thread::sleep(left.min(Duration::from_millis(1)));
        };
        Ok(ProcessOutput {
            status,
            stdout: capture(&mut self.stdout)?,
            stderr: capture(&mut self.stderr)?,
        })
    }
}

fn capture(file: &mut File) -> io::Result<Vec<u8>> {
    let len = file.metadata()?.len();
    if len > MAX_PROCESS_OUTPUT as u64 {
        return Err(io::Error::new(
            ErrorKind::FileTooLarge,
            "process output limit",
        ));
    }
    // The confined child has exited and could not fork; no other writer remains.
    let mut bytes = vec![0; len as usize];
    file.rewind()?;
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

impl Drop for Running {
    fn drop(&mut self) {
        // Seccomp denies process creation. Kill the sole direct child on any
        // timeout/error/unwind. Child caches the status if already waited on.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod process_tests;
