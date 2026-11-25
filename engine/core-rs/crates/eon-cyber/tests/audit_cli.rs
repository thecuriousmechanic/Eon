use serde_json::Value;
use std::io::Write;
use std::process::{Command, Output, Stdio};

fn audit(mode: &str, input: &[u8]) -> Output {
    invoke(&[mode], input)
}

fn invoke(args: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_eon-audit"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn reports_byte_ranges_but_never_reflects_stdin() {
    let src = "def sample(db, val):\n    db.execute(f'private_marker {val}')\n";
    let output = audit("python", src.as_bytes());
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert!(
        !std::str::from_utf8(&output.stdout)
            .unwrap()
            .contains("private_marker")
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report.get("etw").is_none());
    let sql = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["cwe"] == "CWE-89")
        .unwrap();
    assert_eq!(sql["status"], "observed_syntax");
    assert_eq!(
        &src[sql["start_byte"].as_u64().unwrap() as usize
            ..sql["end_byte"].as_u64().unwrap() as usize],
        "db.execute(f'private_marker {val}')"
    );
}

#[test]
fn etw_is_opt_in_for_reviews_only_and_does_not_change_findings() {
    let source = b"def f():\n    return 'private_marker'\n";
    let plain = audit("python", source);
    let opted_in = invoke(&["python", "--etw"], source);
    assert!(plain.status.success() && opted_in.status.success());
    let plain: Value = serde_json::from_slice(&plain.stdout).unwrap();
    let mut with_marker: Value = serde_json::from_slice(&opted_in.stdout).unwrap();
    let marker = with_marker.as_object_mut().unwrap().remove("etw").unwrap();
    assert_eq!(plain, with_marker);
    assert!(!String::from_utf8_lossy(&opted_in.stdout).contains("private_marker"));
    #[cfg(not(windows))]
    assert_eq!(
        marker,
        serde_json::json!({"status": "unverified", "reason": "Unsupported"})
    );
    #[cfg(windows)]
    assert!(matches!(
        marker["status"].as_str(),
        Some("submitted" | "unverified")
    ));
    for args in [
        &["self-io", "--etw"][..],
        &["rust", "--etw", "target"][..],
        &["python", "--unknown"][..],
    ] {
        let refused = invoke(args, b"");
        assert_eq!(refused.status.code(), Some(2));
        assert!(refused.stdout.is_empty());
    }
}

#[test]
fn malformed_source_is_not_reported_clean() {
    let output = audit("rust", b"fn private_marker(");
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["status"] == "unverified")
    );
    for data in [vec![0xff; 4], vec![b'a'; 16 * 1024 + 1]] {
        let refused = audit("rust", &data);
        assert_eq!(refused.status.code(), Some(2));
        assert!(refused.stdout.is_empty());
    }
}

#[test]
fn self_io_reports_only_numeric_evidence_or_a_gap() {
    let output = audit("self-io", &[]);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(output.status.success());
    assert_eq!(report["source"], "proc_self_io");
    if cfg!(target_os = "linux") && std::fs::read_to_string("/proc/self/io").is_ok() {
        assert_eq!(report["status"], "observed");
        assert!(report["read_bytes"].is_u64() && report["write_bytes"].is_u64());
    } else {
        assert!(report["status"] == "unverified" || report["status"] == "observed");
    }
}

#[test]
fn self_cpu_is_opt_in_for_reviews_only_and_preserves_findings() {
    let source = b"def f():\n    return 'private_marker'\n";
    let plain = audit("python", source);
    let measured = invoke(&["python", "--self-cpu-ebpf"], source);
    assert!(plain.status.success() && measured.status.success());
    let plain: Value = serde_json::from_slice(&plain.stdout).unwrap();
    assert!(plain.get("self_thread_user_cpu_ebpf").is_none());
    assert!(!String::from_utf8_lossy(&measured.stdout).contains("private_marker"));
    let mut report: Value = serde_json::from_slice(&measured.stdout).unwrap();
    let cpu = report
        .as_object_mut()
        .unwrap()
        .remove("self_thread_user_cpu_ebpf")
        .unwrap();
    assert_eq!(plain, report);
    match cpu["status"].as_str() {
        Some("observed") => assert!(cpu["samples"].as_u64().unwrap() > 0),
        Some("unverified") => assert!(cpu.get("samples").is_none()),
        status => panic!("unexpected status: {status:?}"),
    }
    for args in [
        &["self-io", "--self-cpu-ebpf"][..],
        &["python", "--self-cpu-ebpf", "target"][..],
        &["python", "--self-cpu-ebpf", "--etw"][..],
    ] {
        let refused = invoke(args, b"");
        assert_eq!(refused.status.code(), Some(2));
        assert!(refused.stdout.is_empty());
    }
}
