use std::io::ErrorKind;
use std::path::Path;

use super::*;

// /proc also contains zombies. Disappearance verifies reaping, not just
// signal delivery. Nothing in these fixtures opens a sound device.
fn assert_reaped(pid: u32) {
    let path = format!("/proc/{pid}");
    let deadline = Instant::now() + Duration::from_secs(2);
    while Path::new(&path).exists() {
        assert!(Instant::now() < deadline, "child {pid} was not reaped");
        std::thread::sleep(POLL);
    }
}

fn sleeper() -> Command {
    let mut command = Command::new("/bin/sleep");
    command.arg("10");
    command
}

#[test]
fn deadline_stops_a_child_with_both_io_directions_blocked() {
    let (mut child, mut input, output) = spawn(sleeper()).unwrap();
    let pid = child.id();
    // Force backpressure even on hosts where the entire production input fits
    // into the initial buffer. sleep never reads or writes either endpoint.
    let start = Instant::now();
    loop {
        match input.write(&[0; 4096]) {
            Ok(n) => assert_ne!(n, 0),
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            other => panic!("unexpected write result {other:?}"),
        }
        assert!(start.elapsed() < Duration::from_secs(1), "could not fill stdin");
    }
    let budget = Duration::from_millis(80);
    let start = Instant::now();
    let err = communicate(&mut child, input, output, b"still pending", start + budget, &|| false).unwrap_err();
    drop(child);
    assert!(err.to_string().contains("deadline exceeded"), "{err}");
    assert!(start.elapsed() >= budget && start.elapsed() < Duration::from_secs(1));
    assert_reaped(pid);
}

#[test]
fn cancellation_stops_and_reaps_an_already_started_child() {
    let (mut child, input, output) = spawn(sleeper()).unwrap();
    let pid = child.id();
    let polls = std::cell::Cell::new(0);
    let start = Instant::now();
    let err = communicate(&mut child, input, output, b"hello", start + BUDGET, &|| {
        polls.set(polls.get() + 1);
        polls.get() == 3
    })
    .unwrap_err();
    drop(child);
    assert!(err.to_string().contains("cancelled"), "{err}");
    assert!(start.elapsed() < Duration::from_secs(1));
    assert_reaped(pid);
}

#[test]
fn overflowing_stdout_fails_without_returning_a_truncated_pronunciation() {
    let (mut child, input, output) = spawn(Command::new("/usr/bin/yes")).unwrap();
    let pid = child.id();
    let err = communicate(&mut child, input, output, b"", Instant::now() + BUDGET, &|| false).unwrap_err();
    drop(child);
    assert!(err.to_string().contains("stdout exceeds byte budget"), "{err}");
    assert_reaped(pid);
}

#[test]
fn exact_stdout_limit_is_accepted() {
    let (mut child, input, output) = spawn(Command::new("/bin/cat")).unwrap();
    let text = vec![b'x'; MAX_STDOUT];
    let bytes = communicate(&mut child, input, output, &text, Instant::now() + BUDGET, &|| false).unwrap();
    assert_eq!(bytes, text);
    assert!(child.reaped());
}

#[test]
fn stdout_without_eof_is_bounded_even_after_exit() {
    let (mut child, input, _) = spawn(Command::new("/bin/true")).unwrap();
    // Simulate an inherited descriptor: even successful process exit does not
    // make an empty stdout readable. No orphan helper process is needed.
    let (output, _held_open) = std::os::unix::net::UnixStream::pair().unwrap();
    output.set_nonblocking(true).unwrap();
    let err = communicate(
        &mut child,
        input,
        output,
        b"",
        Instant::now() + Duration::from_millis(80),
        &|| false,
    )
    .unwrap_err();
    assert!(err.to_string().contains("deadline exceeded"), "{err}");
    assert!(child.reaped());
}
