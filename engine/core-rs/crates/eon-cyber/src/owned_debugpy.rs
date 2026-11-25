//! A single opt-in CPython counting fixture, not a general debugger client.

use crate::owned_dap::{Error, SourceFrame, Visit};
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/owned_debugpy");
const DRIVER: &str = include_str!("../fixtures/owned_debugpy/probe.py");
static RUN: Mutex<()> = Mutex::new(());

// This fixed script runs only after unshare has successfully created all
// namespaces. It has no caller-supplied command, shell fragment or path.
const INIT: &str = r#"
set -eu
/usr/sbin/ip link set dev lo up
/usr/bin/mount -t tmpfs -o size=48m,nosuid,nodev,noexec,mode=700 tmpfs "$1"
cd "$1"
exec /usr/bin/setpriv --no-new-privs --bounding-set=-all --inh-caps=-all --ambient-caps=-all /usr/bin/python3 -ISB -c "$2"
"#;

unsafe extern "C" {
    fn close_range(first: u32, last: u32, flags: i32) -> i32;
    fn prctl(option: i32, arg2: usize, arg3: usize, arg4: usize, arg5: usize) -> i32;
    fn getppid() -> i32;
}

struct Namespace(Child);

impl Drop for Namespace {
    fn drop(&mut self) {
        // unshare --kill-child ties PID 1 to this unreaped direct child.
        // Losing PID 1 kills every descendant, including separate groups.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn launch(visit: Visit) -> Result<(Namespace, UnixStream), Error> {
    let parent = std::process::id() as i32;
    let net = std::fs::metadata("/proc/self/ns/net").map_err(|_| Error::Unavailable)?;
    let (client, init) = UnixStream::pair().map_err(|_| Error::Transport)?;
    let stdin = init.try_clone().map_err(|_| Error::Transport)?;
    let mut cmd = Command::new("/usr/bin/unshare");
    cmd.args([
        "--user",
        "--map-root-user",
        "--net",
        "--pid",
        "--fork",
        "--kill-child=SIGKILL",
        "--mount-proc",
        "--propagation",
        "private",
        "/bin/sh",
        "-c",
        INIT,
        "owned-debugpy",
        DIR,
        DRIVER,
    ])
    .env_clear()
    .env("HOME", DIR)
    .env("TMPDIR", DIR)
    .env("PATH", "/usr/bin")
    .env("OWNED_PARENT_NET", net.ino().to_string())
    .env(
        "OWNED_VISIT",
        match visit {
            Visit::Second => "2",
            Visit::Third => "3",
        },
    )
    .current_dir("/")
    .stdin(Stdio::from(OwnedFd::from(stdin)))
    .stdout(Stdio::from(OwnedFd::from(init)))
    .stderr(Stdio::null());
    // SAFETY: Linux syscalls only, no allocation or locks after fork. Mark
    // inherited descriptors CLOEXEC (including Rust's exec error pipe).
    unsafe {
        cmd.pre_exec(move || {
            if close_range(3, u32::MAX, 4) != 0 || prctl(1, 9, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if getppid() != parent {
                return Err(std::io::Error::from_raw_os_error(1));
            }
            Ok(())
        });
    }
    let child = cmd.spawn().map_err(|_| Error::Unavailable)?;
    Ok((Namespace(child), client))
}

fn finish(ns: &mut Namespace, deadline: Instant) -> Result<(), Error> {
    loop {
        if Instant::now() >= deadline {
            return Err(Error::Deadline);
        }
        if let Some(status) = ns.0.try_wait().map_err(|_| Error::Transport)? {
            return if status.success() {
                Ok(())
            } else {
                Err(match status.code() {
                    Some(65) => Error::Protocol,
                    Some(66) => Error::UnexpectedFixture,
                    _ => Error::Unavailable,
                })
            };
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn read_evidence(
    ns: &mut Namespace,
    stream: &mut UnixStream,
    deadline: Instant,
    mut remaining: &mut [u8],
) -> Result<(), Error> {
    while !remaining.is_empty() {
        let time = deadline
            .checked_duration_since(Instant::now())
            .filter(|t| !t.is_zero())
            .ok_or(Error::Deadline)?;
        stream
            .set_read_timeout(Some(time))
            .map_err(|_| Error::Transport)?;
        match stream.read(remaining) {
            Ok(0) => {
                finish(ns, deadline)?;
                return Err(Error::Protocol);
            }
            Ok(n) => remaining = &mut remaining[n..],
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Err(Error::Deadline);
            }
            Err(_) => return Err(Error::Transport),
        }
    }
    Ok(())
}

fn paused(
    ns: &mut Namespace,
    stream: &mut UnixStream,
    deadline: Instant,
    visit: Visit,
) -> Result<[SourceFrame; 3], Error> {
    let mut bytes = [0; 6];
    read_evidence(ns, stream, deadline, &mut bytes)?;
    let (expected, caller) = match visit {
        Visit::Second => ([1, 3, 2, 9, 4, 16], "second"),
        Visit::Third => ([1, 3, 3, 12, 4, 17], "third"),
    };
    if bytes != expected {
        return Err(Error::UnexpectedFixture);
    }
    let functions = ["count", caller, "main"];
    Ok(std::array::from_fn(|i| SourceFrame {
        function: functions[i],
        line: u32::from(bytes[2 * i + 1]),
    }))
}

/// Returns only the top three verified physical source locations. Requires
/// unprivileged user/net/PID/mount namespaces and the SHA-256-pinned PyPI
/// `debugpy-1.8.22-py2.py3-none-any.whl` in `engine/core-rs/target/`.
/// There is no download, sudo, host-network fallback, or persistent workspace.
pub fn inspect_counting_fixture(visit: Visit) -> Result<[SourceFrame; 3], Error> {
    run(visit, false).map(|(frames, _)| frames)
}

/// Cumulative successful `Marker` constructions and live entries in the
/// fixture's weak registry. Neither measures the interpreter's whole heap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MarkerCounters {
    pub allocations: u8,
    pub live: u8,
}

#[derive(Debug, Eq, PartialEq)]
pub struct CountingObservation {
    pub frames: [SourceFrame; 3],
    pub counters: MarkerCounters,
}

/// Also read the marker counters from the verified `count` frame. The
/// ordinary stack-only entry point never asks for scopes or variables.
pub fn observe_counting_fixture(visit: Visit) -> Result<CountingObservation, Error> {
    let (frames, counters) = run(visit, true)?;
    Ok(CountingObservation {
        frames,
        counters: counters.ok_or(Error::Protocol)?,
    })
}

fn run(visit: Visit, sample: bool) -> Result<([SourceFrame; 3], Option<MarkerCounters>), Error> {
    let _run = RUN.try_lock().map_err(|_| Error::Busy)?;
    let deadline = Instant::now() + Duration::from_secs(15);
    let (mut ns, mut stream) = launch(visit)?;
    let frames = paused(&mut ns, &mut stream, deadline, visit)?;
    let counters = if sample {
        stream.write_all(b"M").map_err(|_| Error::Transport)?;
        let mut bytes = [0; 2];
        read_evidence(&mut ns, &mut stream, deadline, &mut bytes)?;
        let [allocations, live] = bytes;
        if allocations > 3 || live > allocations {
            return Err(Error::UnexpectedFixture);
        }
        Some(MarkerCounters { allocations, live })
    } else {
        None
    };
    stream.write_all(b"C").map_err(|_| Error::Transport)?;
    finish(&mut ns, deadline)?;
    Ok((frames, counters))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires private unprivileged namespaces and pinned debugpy wheel"]
    fn marker_lifetimes_at_each_real_breakpoint() {
        for (visit, allocations, live) in [(Visit::Second, 2, 1), (Visit::Third, 3, 2)] {
            let obs = observe_counting_fixture(visit).unwrap();
            assert_eq!(obs.counters, MarkerCounters { allocations, live });
            println!("{visit:?}: {obs:?}");
        }
    }

    #[test]
    #[ignore = "requires private unprivileged namespaces and pinned debugpy wheel"]
    fn cancellation_destroys_paused_namespace() {
        let end = Instant::now() + Duration::from_secs(15);
        let (mut ns, mut stream) = launch(Visit::Second).unwrap();
        paused(&mut ns, &mut stream, end, Visit::Second).unwrap();
        // Only PID 1 holds the private peer once unshare and init are gone.
        // Keep our end open, without ever giving the permission to resume.
        drop(ns);
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        assert_eq!(stream.read(&mut [0]).unwrap(), 0);
    }
}
