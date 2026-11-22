use std::io::{self, Read, Write};
use std::process::Stdio;

/// Return nonblocking parent ends and **blocking** child ends. Thus neither
/// feeding stdin nor draining stdout can outlive a cancellation or deadline.
#[cfg(unix)]
pub(super) fn stdio() -> io::Result<(impl Write, Stdio, impl Read, Stdio)> {
    use std::os::{fd::OwnedFd, unix::net::UnixStream};

    let (input, stdin) = UnixStream::pair()?;
    let (output, stdout) = UnixStream::pair()?;
    input.set_nonblocking(true)?;
    output.set_nonblocking(true)?;
    Ok((input, OwnedFd::from(stdin).into(), output, OwnedFd::from(stdout).into()))
}

#[cfg(windows)]
pub(super) fn stdio() -> io::Result<(impl Write, Stdio, impl Read, Stdio)> {
    use std::os::windows::io::{AsRawHandle, RawHandle};
    use std::ptr::null;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetNamedPipeHandleState(handle: RawHandle, mode: *const u32, count: *const u32, timeout: *const u32) -> i32;
    }

    fn nowait(handle: &impl AsRawHandle) -> io::Result<()> {
        const PIPE_NOWAIT: u32 = 1;
        // SAFETY: io::pipe owns a live byte-mode named pipe handle; only its
        // parent end changes wait mode. The two optional collection settings are unused.
        if unsafe { SetNamedPipeHandleState(handle.as_raw_handle(), &PIPE_NOWAIT, null(), null()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    let (stdin, input) = io::pipe()?;
    let (output, stdout) = io::pipe()?;
    nowait(&input)?;
    nowait(&output)?;
    Ok((input, stdin.into(), output, stdout.into()))
}

#[cfg(not(any(unix, windows)))]
pub(super) fn stdio() -> io::Result<(io::Sink, Stdio, io::Empty, Stdio)> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "nonblocking espeak stdio unavailable",
    ))
}
