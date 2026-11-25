use super::*;
use eon_foundation::journal::JournalWriter;

fn key(state: &mut WatchState, code: KeyCode) {
    state.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
}

fn type_text(state: &mut WatchState, text: &str) {
    for c in text.chars() {
        state.on_key(c);
    }
}

fn screen(state: &WatchState) -> String {
    render_watch_snapshot(state, 90, 14).unwrap()
}

#[test]
fn menu_navigation_and_completion_use_the_same_destinations() {
    let mut state = WatchState::default();
    assert!(screen(&state).contains("> /status"));
    key(&mut state, KeyCode::Down);
    key(&mut state, KeyCode::Enter);
    assert!(screen(&state).contains("What this client can observe"));
    key(&mut state, KeyCode::Esc);
    assert!(screen(&state).contains("> /capabilities"));
    key(&mut state, KeyCode::Up);
    assert!(screen(&state).contains("> /status"));

    type_text(&mut state, "/m");
    key(&mut state, KeyCode::Tab);
    assert!(screen(&state).contains("Match 1/2: /mesh /menu"));
    key(&mut state, KeyCode::Tab);
    assert_eq!(state.prompt.as_ref().unwrap().text, "/menu");
    key(&mut state, KeyCode::BackTab);
    key(&mut state, KeyCode::Enter);
    assert!(screen(&state).contains("Waiting for the local journal"));

    type_text(&mut state, "/v");
    key(&mut state, KeyCode::Tab);
    key(&mut state, KeyCode::Enter);
    assert!(screen(&state).contains("No vision frame source"));
    type_text(&mut state, "/menu");
    key(&mut state, KeyCode::Enter);
    assert!(screen(&state).contains("Welcome - command menu"));
}

#[test]
fn editing_rejects_unknown_commands_and_does_not_quit_or_dispatch_on_completion() {
    let mut state = WatchState::default();
    type_text(&mut state, "/q");
    key(&mut state, KeyCode::Tab);
    assert!(screen(&state).contains("No matching command"));
    assert!(!state.quit);
    key(&mut state, KeyCode::Backspace);
    type_text(&mut state, "sta");
    key(&mut state, KeyCode::Tab);
    assert_eq!(state.prompt.as_ref().unwrap().text, "/status");
    type_text(&mut state, "; ls");
    key(&mut state, KeyCode::Enter);
    assert!(screen(&state).contains("Unknown command"));
    assert_eq!(state.page, Page::Menu);
    key(&mut state, KeyCode::Esc);
    assert!(state.prompt.is_none());
    state.handle_key(KeyEvent::new_with_kind(
        KeyCode::Char('q'),
        KeyModifiers::NONE,
        KeyEventKind::Release,
    ));
    assert!(!state.quit);
    state.on_key('q');
    assert!(state.quit);
}

#[test]
fn replay_drives_dock_integrity_and_recent_metadata_through_failure_and_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let segment = dir.path().join("seg_00000000000000000001.eonj");
    std::fs::write(dir.path().join("ignored.eonj"), b"not a segment").unwrap();
    let mut state = WatchState::default();
    state.refresh(dir.path());
    type_text(&mut state, "/cyber");
    key(&mut state, KeyCode::Enter);
    assert!(screen(&state).contains("No frames to check"));
    let mut writer = JournalWriter::open(dir.path()).unwrap();
    writer.append_batch(&[b"private", b"\x00\xff"]).unwrap();
    drop(writer);
    state.refresh(dir.path());
    let text = screen(&state);
    assert!(text.contains("checksums and sequence for 2 frames"));
    assert!(text.contains("segments: 1 | frames: 2 | last durable seq: 2"));

    type_text(&mut state, "/status");
    key(&mut state, KeyCode::Enter);
    let text = screen(&state);
    assert!(
        text.find("seq 2: 2 payload bytes").unwrap() < text.find("seq 1: 7 payload bytes").unwrap()
    );
    assert!(!text.contains("private"));

    let good = std::fs::read(&segment).unwrap();
    std::fs::write(&segment, b"invalid").unwrap();
    state.refresh(dir.path());
    assert!(screen(&state).contains("Replay failed:"));
    assert!(!screen(&state).contains("frames: 2"));
    type_text(&mut state, "/cyber");
    key(&mut state, KeyCode::Enter);
    assert!(!screen(&state).contains("Replay validated"));
    assert_eq!(std::fs::read(&segment).unwrap(), b"invalid");
    std::fs::write(segment, good).unwrap();
    state.refresh(dir.path());
    assert!(screen(&state).contains("checksums and sequence for 2 frames"));
}

#[test]
fn mesh_replay_counts_only_complete_known_markers_and_clears_on_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = JournalWriter::open(dir.path()).unwrap();
    let marker = PairedSyncCompleted.encode();
    let mut unknown_version = marker.to_vec();
    *unknown_version.last_mut().unwrap() += 1;
    writer
        .append_batch(&[
            b"ordinary event",
            &marker[..marker.len() - 1],
            &unknown_version,
            &[marker, b"suffix"].concat(),
        ])
        .unwrap();
    let mut state = WatchState::default();
    type_text(&mut state, "/mesh");
    key(&mut state, KeyCode::Enter);
    state.refresh(dir.path());
    assert!(screen(&state).contains("No completed paired syncs recorded"));

    writer.append_batch(&[marker, marker]).unwrap();
    writer.append_batch(&[b"unrelated".as_slice(); 9]).unwrap();
    state.refresh(dir.path());
    let text = screen(&state);
    assert!(text.contains("Completed paired syncs recorded: 2"));
    assert!(text.contains("Last sync record at journal seq: 6"));

    state.refresh(&dir.path().join("missing"));
    let text = screen(&state);
    assert!(text.contains("Journal replay failed; paired sync observations unavailable"));
    assert!(!text.contains("Completed paired syncs recorded:"));
    assert!(!text.contains("Last sync record at journal seq:"));
    state.refresh(dir.path());
    assert!(screen(&state).contains("Completed paired syncs recorded: 2"));
}

#[test]
fn small_terminals_and_refresh_do_not_lose_the_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = WatchState::default();
    type_text(&mut state, "/cap");
    state.refresh(dir.path());
    key(&mut state, KeyCode::Tab);
    assert!(
        render_watch_snapshot(&state, 8, 4)
            .unwrap()
            .contains("ilities")
    );
    render_watch_snapshot(&state, 1, 1).unwrap();
    key(&mut state, KeyCode::Enter);
    assert!(screen(&state).contains("Read-only local journal: readable"));
}
