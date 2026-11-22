//! Runs the compiled `eon` binary against a journal written by the foundation crate.

use std::process::Command;

use eon_foundation::journal::JournalWriter;

#[test]
fn binary_reports_the_durable_journal_contents() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = JournalWriter::open(dir.path()).unwrap();
    for i in 0..7 {
        writer
            .append_batch(&[format!("event {i}").as_bytes()])
            .unwrap();
    }
    drop(writer);

    let output = Command::new(env!("CARGO_BIN_EXE_eon"))
        .args(["status", dir.path().to_str().unwrap()])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("frames: 7"), "stdout: {stdout}");
    assert!(stdout.contains("last durable seq: 7"), "stdout: {stdout}");
}

#[test]
fn binary_rejects_missing_arguments_with_usage_exit_code() {
    let output = Command::new(env!("CARGO_BIN_EXE_eon")).output().unwrap();
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn watch_refuses_redirected_io_without_writing_terminal_controls() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_eon"))
        .arg("watch")
        .arg(dir.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires a terminal"));
}
