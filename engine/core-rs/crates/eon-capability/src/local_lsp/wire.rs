use std::io::{self, BufReader, Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::process::{ChildStdin, ChildStdout};
use std::time::Instant;

use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use serde_json::Value;

use super::{invalid, timeout};

const HEADER_LIMIT: usize = 1024;
const FRAME_LIMIT: usize = 512 * 1024;

fn nonblocking(fd: impl AsFd) -> io::Result<()> {
    let flags = fcntl_getfl(&fd)?;
    Ok(fcntl_setfl(fd, flags | OFlags::NONBLOCK)?)
}

fn until<T>(fd: i32, events: i16, end: Instant, mut f: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    loop {
        if Instant::now() >= end {
            return Err(timeout());
        }
        match f() {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => (),
            result => return result,
        }
        let ms = end.saturating_duration_since(Instant::now()).as_micros().div_ceil(1000);
        let mut p = libc::pollfd { fd, events, revents: 0 };
        // SAFETY: fd is borrowed for this operation. p is one writable pollfd.
        let n = unsafe { libc::poll(&mut p, 1, ms.min(i32::MAX as u128) as i32) };
        if n == 0 {
            return Err(timeout());
        }
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }
}

fn content_length(header: &[u8]) -> io::Result<usize> {
    let mut length = None;
    let mut content_type_seen = false;
    for line in std::str::from_utf8(header)
        .map_err(|_| invalid("non-ASCII header"))?
        .split("\r\n")
    {
        if !line.bytes().all(|b| matches!(b, b' '..=b'~')) {
            return Err(invalid("invalid header character"));
        }
        let (key, value) = line.split_once(':').ok_or_else(|| invalid("invalid header"))?;
        if key.eq_ignore_ascii_case("Content-Length") && length.is_none() {
            let digits = value.trim();
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return Err(invalid("invalid length"));
            }
            length = digits.parse::<usize>().ok();
            if !matches!(length, Some(1..=FRAME_LIMIT)) {
                return Err(invalid("invalid frame size"));
            }
        } else if key.eq_ignore_ascii_case("Content-Type") && !content_type_seen {
            content_type_seen = true;
        } else {
            return Err(invalid("unknown or duplicate header"));
        }
    }
    length.ok_or_else(|| invalid("missing length"))
}

pub(super) struct Wire {
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Wire {
    #[cfg(test)]
    pub(super) fn limit_test_pipe(&self) {
        // Make a partial-write test independent of the host's default pipe size.
        // SAFETY: this is our live child-stdin fd; F_SETPIPE_SZ takes a byte count.
        let size = unsafe { libc::fcntl(self.input.as_raw_fd(), libc::F_SETPIPE_SZ, 4096) };
        assert!((1..=64 * 1024).contains(&size), "unexpected test pipe size: {size}");
    }

    pub fn new(input: ChildStdin, output: ChildStdout) -> io::Result<Self> {
        nonblocking(&input)?;
        nonblocking(&output)?;
        Ok(Self {
            input,
            output: BufReader::new(output),
        })
    }

    pub fn send(&mut self, value: &Value, end: Instant) -> io::Result<()> {
        // The only callers serialize fixed shapes with bounded strings.
        let body = serde_json::to_vec(value)?;
        if body.len() > FRAME_LIMIT {
            return Err(invalid("outgoing frame too large"));
        }
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        for mut bytes in [header.as_bytes(), &body] {
            while !bytes.is_empty() {
                let n = until(self.input.as_raw_fd(), libc::POLLOUT, end, || self.input.write(bytes))?;
                if n == 0 {
                    return Err(io::ErrorKind::WriteZero.into());
                }
                bytes = &bytes[n..];
            }
        }
        Ok(())
    }

    fn fill(&mut self, mut bytes: &mut [u8], end: Instant) -> io::Result<()> {
        while !bytes.is_empty() {
            let n = until(self.output.get_ref().as_raw_fd(), libc::POLLIN, end, || {
                self.output.read(bytes)
            })?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            bytes = &mut bytes[n..];
        }
        Ok(())
    }

    /// Before serving a cached push, check for already queued updates (or EOF).
    /// Once started, reading a partial frame still uses the operation deadline.
    pub fn readable(&self, end: Instant) -> io::Result<bool> {
        if !self.output.buffer().is_empty() {
            return Ok(true);
        }
        let mut p = libc::pollfd {
            fd: self.output.get_ref().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            // SAFETY: p describes one live borrowed descriptor and zero timeout.
            let n = unsafe { libc::poll(&mut p, 1, 0) };
            if n >= 0 {
                return Ok(n != 0);
            }
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(err);
            }
            if Instant::now() >= end {
                return Err(timeout());
            }
        }
    }

    pub fn recv(&mut self, end: Instant) -> io::Result<Value> {
        let mut header = Vec::new();
        loop {
            if header.len() == HEADER_LIMIT {
                return Err(invalid("header too large"));
            }
            let mut byte = [0];
            self.fill(&mut byte, end)?;
            header.push(byte[0]);
            if header.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let mut body = vec![0; content_length(&header[..header.len() - 4])?];
        self.fill(&mut body, end)?;
        Ok(serde_json::from_slice(&body)?)
    }
}

#[cfg(test)]
mod lsp_and_patcher_tests {
    use super::*;

    #[test]
    fn length_is_unique_decimal_and_bounded_before_allocating() {
        assert_eq!(
            content_length(b"content-length: 20\r\nContent-Type: application/vscode-jsonrpc").unwrap(),
            20
        );
        for bad in [
            "Content-Length: 1\r\nContent-Length: 2",
            "Content-Length: +4",
            "Content-Length: 524289",
            "Content-Length: 0",
            "Other: 2",
            "Content-Length: 1\nX",
            "Content-Length: 18446744073709551616",
        ] {
            assert!(content_length(bad.as_bytes()).is_err());
        }
    }
}
