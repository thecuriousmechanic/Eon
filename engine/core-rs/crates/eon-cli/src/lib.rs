//! Read-only journal status and its terminal rendering (docs/implementation/05, section 5).
//!
//! The status is computed from the journal on disk, so the CLI shows what is durable rather than what a process
//! believes. Rendering goes through ratatui, and the same widget is used by the binary and by tests.

use std::fs;
use std::path::Path;

use eon_foundation::journal::{Frame, replay};
use eon_primitives::{EonError, EonResult};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::widgets::{Block, Borders, Paragraph};

mod watch;
pub use watch::{WatchState, render_watch_snapshot, run_watch, run_watch_with_image};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalStatus {
    pub segments: usize,
    pub frames: usize,
    pub last_seq: Option<u64>,
}

fn read_journal(dir: &Path) -> EonResult<(JournalStatus, Vec<Frame>)> {
    let read_error = |e: std::io::Error| EonError::OperationFailed {
        operation: "status.read_dir".into(),
        reason: e.to_string(),
    };
    let mut segments = 0;
    for entry in fs::read_dir(dir).map_err(read_error)? {
        // Count only segment names that foundation's journal replay recognizes.
        let name = entry.map_err(read_error)?.file_name();
        if name
            .to_str()
            .and_then(|s| s.strip_prefix("seg_"))
            .and_then(|s| s.strip_suffix(".eonj"))
            .and_then(|s| s.parse::<u64>().ok())
            .is_some()
        {
            segments += 1;
        }
    }
    let frames = replay(dir)?;
    let status = JournalStatus {
        segments,
        frames: frames.len(),
        last_seq: frames.last().map(|f| f.seq_no),
    };
    Ok((status, frames))
}

pub fn read_status(dir: &Path) -> EonResult<JournalStatus> {
    read_journal(dir).map(|(status, _)| status)
}

pub fn render_text(status: &JournalStatus) -> String {
    let last = status
        .last_seq
        .map_or("none".to_string(), |s| s.to_string());
    format!(
        "segments: {}\nframes: {}\nlast durable seq: {}\n",
        status.segments, status.frames, last
    )
}

fn draw_body(frame: &mut ratatui::Frame, title: &str, body: &str) {
    let widget = Paragraph::new(body.to_owned()).block(
        Block::default()
            .borders(Borders::ALL)
            .title(title.to_owned()),
    );
    frame.render_widget(
        widget,
        Rect::new(0, 0, frame.area().width, frame.area().height),
    );
}

fn draw(frame: &mut ratatui::Frame, status: &JournalStatus) {
    draw_body(frame, " EON journal ", &render_text(status));
}

/// Renders the status into an in-memory terminal and returns the visible text, one line per row.
pub fn render_snapshot(status: &JournalStatus, width: u16, height: u16) -> EonResult<String> {
    snapshot(width, height, |frame| draw(frame, status))
}

fn snapshot(width: u16, height: u16, draw: impl FnOnce(&mut ratatui::Frame)) -> EonResult<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height))
        .map_err(|e| EonError::InternalError(format!("terminal: {e}")))?;
    terminal
        .draw(draw)
        .map_err(|e| EonError::InternalError(format!("draw: {e}")))?;
    let buffer = terminal.backend().buffer();
    let lines: Vec<String> = (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect();
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use eon_foundation::journal::JournalWriter;

    use super::*;

    #[test]
    fn status_reflects_what_is_durable_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = JournalWriter::open_with_segment_size(dir.path(), 128).unwrap();
        for i in 0..20 {
            writer
                .append_batch(&[format!("event {i}").as_bytes()])
                .unwrap();
        }
        drop(writer);
        let status = read_status(dir.path()).unwrap();
        assert_eq!(status.frames, 20);
        assert_eq!(status.last_seq, Some(20));
        assert!(status.segments > 1);
    }

    #[test]
    fn empty_journal_reports_no_durable_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let status = read_status(dir.path()).unwrap();
        assert_eq!(
            status,
            JournalStatus {
                segments: 0,
                frames: 0,
                last_seq: None
            }
        );
        assert!(render_text(&status).contains("last durable seq: none"));
    }

    #[test]
    fn terminal_snapshot_shows_the_counts() {
        let status = JournalStatus {
            segments: 2,
            frames: 41,
            last_seq: Some(41),
        };
        let screen = render_snapshot(&status, 40, 6).unwrap();
        assert!(screen.contains("EON journal"));
        assert!(screen.contains("frames: 41"));
        assert!(screen.contains("last durable seq: 41"));
    }

    #[test]
    fn only_q_requests_exit_from_the_watch_view() {
        let mut state = WatchState::default();
        state.on_key('x');
        assert!(!state.quit);
        state.on_key('Q');
        assert!(state.quit);
    }

    #[test]
    fn watch_refresh_reads_the_durable_journal_and_reports_missing_directories() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = JournalWriter::open(dir.path()).unwrap();
        writer
            .append_batch(&[b"a".as_slice(), b"b".as_slice()])
            .unwrap();
        drop(writer);

        let mut state = WatchState::default();
        state.refresh(dir.path());
        assert_eq!(state.status.as_ref().map(|s| s.frames), Some(2));
        assert!(state.error.is_none());

        let mut missing = WatchState::default();
        missing.refresh(&dir.path().join("does-not-exist"));
        assert!(missing.status.is_none());
        assert!(missing.error.is_some());
    }

    #[test]
    fn watch_snapshot_shows_status_or_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = JournalWriter::open(dir.path()).unwrap();
        writer.append_batch(&[b"a".as_slice()]).unwrap();
        drop(writer);

        let mut state = WatchState::default();
        state.refresh(dir.path());
        let text = render_watch_snapshot(&state, 60, 8).unwrap();
        assert!(text.contains("frames: 1"), "{text}");
        assert!(text.contains("press q to quit"), "{text}");

        let mut failed = WatchState::default();
        failed.refresh(&dir.path().join("missing"));
        let text = render_watch_snapshot(&failed, 80, 8).unwrap();
        assert!(text.contains("error:"), "{text}");
    }
}
