use super::*;
use eon_foundation::permissions::Permissions;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

#[test]
fn fixed_modes_run_real_children_on_opaque_supplied_bytes() {
    let auth = IdentityAuthority::new();
    let caller = auth.user(Permissions::EXECUTE_PROCESS);
    let mut provider = ProcessProvider::new(&auth).unwrap();
    let input = b"--help\n/etc/os-release\n$(literal)  text\xff\0";
    for (mode, expected) in [
        (
            ProcessMode::Filter(StreamFilter::AsciiUppercase),
            input.to_ascii_uppercase(),
        ),
        (
            ProcessMode::Filter(StreamFilter::SqueezeSpaces),
            b"--help\n/etc/os-release\n$(literal) text\xff\0".to_vec(),
        ),
        (ProcessMode::CountLines, b"2\n".to_vec()),
        (
            ProcessMode::CountBytes,
            format!("{}\n", input.len()).into_bytes(),
        ),
    ] {
        let out = provider.exec(&caller, mode, input).unwrap();
        assert_eq!(out.status.code(), Some(0));
        assert_eq!(out.stdout, expected);
        assert!(out.stderr.is_empty());
    }
    let empty = provider
        .exec(&caller, ProcessMode::CountLines, b"")
        .unwrap();
    assert_eq!(empty.stdout, b"0\n");
}

#[test]
fn host_grant_required_before_even_validating_input() {
    let auth = IdentityAuthority::new();
    let mut provider = ProcessProvider::new(&auth).unwrap();
    let parent = auth.user(Permissions::EXECUTE_PROCESS | Permissions::SPAWN_SUBAGENT);
    for caller in [
        auth.user(Permissions::READ_WORKSPACE | Permissions::USE_CREDENTIALS),
        IdentityAuthority::new().system(Permissions::EXECUTE_PROCESS),
        parent.delegate(Permissions::NONE).unwrap(),
    ] {
        assert_eq!(
            provider
                .exec(
                    &caller,
                    ProcessMode::CountBytes,
                    &vec![0; MAX_PROCESS_INPUT + 1]
                )
                .err()
                .unwrap()
                .kind(),
            ErrorKind::PermissionDenied
        );
    }
}

#[test]
fn independent_input_and_kernel_output_bounds_and_successful_reuse() {
    let auth = IdentityAuthority::new();
    let caller = auth.user(Permissions::EXECUTE_PROCESS);
    let mut provider = ProcessProvider::new(&auth).unwrap();
    let input = vec![b'a'; MAX_PROCESS_INPUT + 1];
    let upper = ProcessMode::Filter(StreamFilter::AsciiUppercase);
    assert_eq!(
        provider.exec(&caller, upper, &input).err().unwrap().kind(),
        ErrorKind::InvalidInput
    );
    assert_eq!(
        provider
            .exec(&caller, upper, &input[..MAX_PROCESS_INPUT])
            .err()
            .unwrap()
            .kind(),
        ErrorKind::FileTooLarge
    );
    let exact = provider
        .exec(&caller, upper, &input[..MAX_PROCESS_OUTPUT])
        .unwrap();
    assert!(exact.status.success());
    assert_eq!(exact.stdout, vec![b'A'; MAX_PROCESS_OUTPUT]);
    let counted = provider
        .exec(
            &caller,
            ProcessMode::CountBytes,
            &input[..MAX_PROCESS_INPUT],
        )
        .unwrap();
    assert_eq!(counted.stdout, format!("{MAX_PROCESS_INPUT}\n").as_bytes());
}

#[test]
fn worker_cannot_read_a_workspace_file_even_with_a_test_only_operand() {
    // The production argv enum cannot express a file operand. Exercise the
    // same pinned wc under its actual confinement to independently test denial.
    const FILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
    assert!(!fs::read(FILE).unwrap().is_empty());
    let wc = Image::pin(Utility::Wc).unwrap();
    let out = Running::spawn(&wc, &["-c", FILE], Stdio::null())
        .unwrap()
        .finish(Instant::now() + PROCESS_TIMEOUT)
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("Permission denied"));
}

#[test]
fn deadline_kills_and_reaps_confined_child_blocked_on_input() {
    let tr = Image::pin(Utility::Tr).unwrap();
    let (reader, writer) = io::pipe().unwrap(); // Deliberately withhold EOF.
    let running = Running::spawn(&tr, StreamFilter::AsciiUppercase.args(), reader.into()).unwrap();
    let proc = Path::new("/proc").join(running.child.id().to_string());
    crate::pty::pty_tests::assert_child_image(&proc, "/usr/bin/gnutr", "/usr/bin/tr");
    assert_eq!(fs::read(proc.join("cmdline")).unwrap(), b"tr\0a-z\0A-Z\0");
    assert!(fs::read(proc.join("environ")).unwrap().is_empty());
    assert_eq!(fs::read_link(proc.join("cwd")).unwrap(), Path::new("/"));
    for fd in ["fd/1", "fd/2"] {
        assert_eq!(fs::metadata(proc.join(fd)).unwrap().mode() & 0o7777, 0o600);
    }
    let status = fs::read_to_string(proc.join("status")).unwrap();
    assert!(status.lines().any(|s| s == "NoNewPrivs:\t1"));
    assert!(status.lines().any(|s| s == "Seccomp:\t2"));
    let limits = fs::read_to_string(proc.join("limits")).unwrap();
    for (name, cap) in [
        ("Max file size", MAX_PROCESS_OUTPUT as u64 + 1),
        ("Max address space", 64 * 1024 * 1024),
        ("Max core file size", 0),
    ] {
        let line = limits.lines().find_map(|s| s.strip_prefix(name)).unwrap();
        let mut fields = line.split_whitespace();
        let soft: u64 = fields.next().unwrap().parse().unwrap();
        let hard: u64 = fields.next().unwrap().parse().unwrap();
        assert!(soft <= hard && hard <= cap, "{name}: {line}");
    }
    let captures = [&running.stdout, &running.stderr].map(|f| {
        let m = f.metadata().unwrap();
        (m.dev(), m.ino())
    });
    let err = running
        .finish(Instant::now() + Duration::from_millis(30))
        .err()
        .unwrap();
    assert_eq!(err.kind(), ErrorKind::TimedOut);
    assert!(!proc.exists(), "must synchronously reap, not just signal");
    for fd in fs::read_dir("/proc/self/fd").unwrap().flatten() {
        // Other tests may open and close unrelated descriptors concurrently.
        if let Ok(m) = fs::metadata(fd.path()) {
            assert!(!captures.contains(&(m.dev(), m.ino())), "leaked capture fd");
        }
    }
    drop(writer);
}
