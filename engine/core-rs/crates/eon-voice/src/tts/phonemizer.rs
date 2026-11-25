//! A single wall-clock budget covers stdin, stdout, and process exit. Never use
//! `wait_with_output`: even after the child exits, an inherited writer can keep stdout open.

mod child;
mod pipe;
#[cfg(all(test, target_os = "linux"))]
mod tests;

use std::io::{self, Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use eon_primitives::{EonError, EonResult};

use super::{MAX_CLAUSE_BYTES, tts_err};
use child::Running;

const BUDGET: Duration = Duration::from_secs(2);
const MAX_STDOUT: usize = 16 * 1024;
const POLL: Duration = Duration::from_millis(2);

pub(super) fn run(text: &str, cancelled: impl Fn() -> bool) -> EonResult<String> {
    let deadline = Instant::now() + BUDGET;
    if text.len() > MAX_CLAUSE_BYTES {
        return Err(EonError::ValidationError("Kokoro clause exceeds text budget".into()));
    }
    let mut command = Command::new("espeak-ng");
    // Executable search path is operator-owned. No shell, user-selected program,
    // options or filenames; espeak receives only the clause on its standard input.
    command.args(["-v", "en-us", "-q", "--ipa=3"]);
    check_live(deadline, &cancelled)?;
    let (mut child, input, output) = spawn(command)?;
    let bytes = communicate(&mut child, input, output, text.as_bytes(), deadline, &cancelled)?;
    Ok(String::from_utf8_lossy(&bytes)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" "))
}

fn check_live(deadline: Instant, cancelled: &impl Fn() -> bool) -> EonResult<()> {
    if cancelled() {
        Err(tts_err("phonemize", "cancelled"))
    } else if Instant::now() >= deadline {
        Err(tts_err("phonemize", "deadline exceeded"))
    } else {
        Ok(())
    }
}

fn spawn(mut command: Command) -> EonResult<(Running, impl Write, impl Read)> {
    let (input, stdin, output, stdout) = pipe::stdio().map_err(|e| tts_err("phonemize.pipe", e))?;
    let child = Running::spawn(command.stdin(stdin).stdout(stdout).stderr(Stdio::null()))?;
    Ok((child, input, output))
}

fn communicate(
    running: &mut Running,
    input: impl Write,
    mut output: impl Read,
    mut text: &[u8],
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
) -> EonResult<Vec<u8>> {
    let mut input = Some(input);
    let mut bytes = Vec::new();
    let mut eof = false;
    let mut buf = [0; 4096];
    loop {
        check_live(deadline, cancelled)?;
        let mut progress = false;
        if !text.is_empty()
            && let Some(n) = ready(input.as_mut().unwrap().write(text))?
        {
            text = &text[n..];
            progress = n != 0;
        }
        if text.is_empty() {
            input.take(); // EOF is part of the espeak stdin protocol.
        }
        if !eof && let Some(n) = ready(output.read(&mut buf))? {
            if n > MAX_STDOUT - bytes.len() {
                return Err(tts_err("phonemize", "stdout exceeds byte budget"));
            }
            bytes.extend_from_slice(&buf[..n]);
            eof = n == 0;
            progress |= n != 0;
        }
        if let Some(status) = running.try_wait().map_err(|e| tts_err("phonemize.wait", e))?
            && (!status.success() || !text.is_empty())
        {
            return Err(tts_err("phonemize", format!("incomplete pronunciation ({status})")));
        }
        if eof && running.reaped() {
            check_live(deadline, cancelled)?;
            return Ok(bytes);
        }
        if !progress {
            std::thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
        }
    }
}

fn ready(result: io::Result<usize>) -> EonResult<Option<usize>> {
    match result {
        Ok(n) => Ok(Some(n)),
        Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => Ok(None),
        #[cfg(windows)]
        Err(e) if e.raw_os_error() == Some(232) => Ok(None), // ERROR_NO_DATA on a PIPE_NOWAIT handle
        Err(e) => Err(tts_err("phonemize.io", e)),
    }
}
