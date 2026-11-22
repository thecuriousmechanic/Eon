//! Read-only command console for the durable journal. Commands select views; none start processes.

use std::io::{self, IsTerminal};
use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use eon_primitives::{EonError, EonResult, telemetry::PairedSyncCompleted};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::widgets::{Block, List, ListState, Paragraph, Sparkline, Wrap};
use ratatui::{Frame, Terminal};

use crate::{JournalStatus, read_journal};

mod meter;
#[cfg(test)]
mod terminal_tests;
mod vision;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Page {
    #[default]
    Menu,
    Status,
    Capabilities,
    Cyber,
    Mesh,
    Vision,
    Audio,
}

// Menu, dispatch and prefix completion share the same allowlist.
const COMMANDS: &[(Page, &str, &str)] = &[
    (Page::Status, "/status", "Recent durable frame metadata"),
    (
        Page::Capabilities,
        "/capabilities",
        "What this client can observe",
    ),
    (Page::Cyber, "/cyber", "Journal integrity (read-only)"),
    (Page::Mesh, "/mesh", "Recorded paired syncs"),
    (Page::Vision, "/vision", "Selected local image (one still)"),
    (Page::Audio, "/audio", "Local microphone level (opt-in)"),
    (Page::Menu, "/menu", "Command menu"),
];

#[derive(Debug)]
struct Prompt {
    text: String,
    // Keep the original prefix while cycling, not the last completed word.
    prefix: Option<String>,
}

impl Prompt {
    fn complete(&mut self, reverse: bool) -> String {
        let prefix = self.prefix.get_or_insert_with(|| self.text.clone());
        let matches: Vec<_> = COMMANDS
            .iter()
            .map(|&(_, name, _)| name)
            .filter(|name| name.starts_with(prefix.as_str()))
            .collect();
        if matches.is_empty() {
            return "No matching command. Esc cancels.".into();
        }
        let n = matches.len();
        let current = matches.iter().position(|&s| s == self.text);
        let next = match (current, reverse) {
            (None, false) => 0,
            (None, true) => n - 1,
            (Some(i), false) => (i + 1) % n,
            (Some(i), true) => (i + n - 1) % n,
        };
        self.text = matches[next].to_owned();
        format!(
            "Match {}/{}: {}  (Enter to open)",
            next + 1,
            n,
            matches.join(" ")
        )
    }
}

#[derive(Debug)]
struct RecordedSyncs {
    count: usize,
    last_seq: u64,
}

#[derive(Debug, Default)]
pub struct WatchState {
    pub status: Option<JournalStatus>,
    pub error: Option<String>,
    pub quit: bool,
    recent: Vec<(u64, usize)>,
    syncs: Option<RecordedSyncs>,
    page: Page,
    selected: usize,
    prompt: Option<Prompt>,
    feedback: Option<String>,
    meter: meter::Meter,
    image: vision::Selection,
}

impl WatchState {
    pub fn refresh(&mut self, dir: &Path) {
        match read_journal(dir) {
            Ok((status, frames)) => {
                self.status = Some(status);
                let mut syncs = frames
                    .iter()
                    .rev()
                    .filter(|f| PairedSyncCompleted::decode(&f.payload).is_some());
                self.syncs = syncs.next().map(|last| RecordedSyncs {
                    count: 1 + syncs.count(),
                    last_seq: last.seq_no,
                });
                self.recent = frames
                    .iter()
                    .rev()
                    .take(8)
                    .map(|f| (f.seq_no, f.payload.len()))
                    .collect();
                self.error = None;
            }
            Err(err) => {
                self.status = None;
                self.syncs = None;
                self.recent.clear();
                self.error = Some(err.to_string().replace(char::is_control, "?"));
            }
        }
    }

    fn open(&mut self, page: Page) {
        if self.page == Page::Vision && page != Page::Vision {
            self.image.release();
        }
        if page != Page::Audio {
            self.meter.stop();
        }
        self.page = page;
        self.prompt = None;
        self.feedback = None;
        if page != Page::Menu {
            self.selected = COMMANDS.iter().position(|&(p, _, _)| p == page).unwrap();
        }
    }

    pub fn on_key(&mut self, key: char) {
        let code = match key {
            '\r' | '\n' => KeyCode::Enter,
            c => KeyCode::Char(c),
        };
        self.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn handle_event(&mut self, event: Event) {
        // Never reinterpret a paste (even one containing an F2 escape sequence) as keys.
        if let Event::Key(key) = event {
            self.handle_key(key);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
            self.image.release();
            self.meter.stop();
            self.quit = true;
            return;
        }
        if !(key.modifiers - KeyModifiers::SHIFT).is_empty() {
            return;
        }
        if let Some(prompt) = &mut self.prompt {
            match key.code {
                KeyCode::Esc => {
                    self.prompt = None;
                    self.feedback = None;
                }
                KeyCode::Tab | KeyCode::BackTab => {
                    let reverse =
                        key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT);
                    self.feedback = Some(prompt.complete(reverse));
                }
                KeyCode::Backspace => {
                    prompt.text.pop();
                    prompt.prefix = None;
                    self.feedback = None;
                }
                KeyCode::Char(c) if c.is_ascii_graphic() || c == ' ' => {
                    if prompt.text.len() < 96 {
                        prompt.text.push(c);
                        prompt.prefix = None;
                        self.feedback = None;
                    } else {
                        self.feedback = Some("Command length limit reached.".into());
                    }
                }
                KeyCode::Enter => {
                    if let Some(&(page, _, _)) =
                        COMMANDS.iter().find(|&&(_, name, _)| name == prompt.text)
                    {
                        self.open(page);
                    } else {
                        self.feedback =
                            Some("Unknown command. Edit it or press Esc for navigation.".into());
                    }
                }
                _ => {}
            }
        } else {
            match key.code {
                KeyCode::Char('q' | 'Q') => {
                    self.image.release();
                    self.meter.stop();
                    self.quit = true;
                }
                KeyCode::F(2) if self.page == Page::Audio && key.modifiers.is_empty() => {
                    self.meter.toggle();
                }
                KeyCode::Char('/') => {
                    self.meter.stop();
                    self.prompt = Some(Prompt {
                        text: "/".into(),
                        prefix: None,
                    });
                    self.feedback = None;
                }
                KeyCode::Esc => self.open(Page::Menu),
                KeyCode::Up if self.page == Page::Menu => {
                    self.selected = (self.selected + COMMANDS.len() - 1) % COMMANDS.len();
                }
                KeyCode::Down if self.page == Page::Menu => {
                    self.selected = (self.selected + 1) % COMMANDS.len();
                }
                KeyCode::Enter if self.page == Page::Menu => self.open(COMMANDS[self.selected].0),
                _ => {}
            }
        }
    }

    fn dock(&self) -> String {
        if let Some(error) = &self.error {
            format!("journal error: {error}")
        } else if let Some(status) = &self.status {
            let seq = status
                .last_seq
                .map_or_else(|| "none".into(), |n| n.to_string());
            format!(
                "segments: {} | frames: {} | last durable seq: {seq}",
                status.segments, status.frames
            )
        } else {
            "Waiting for the journal".into()
        }
    }

    fn draw(&self, frame: &mut Frame, painter: Option<&mut vision::Painter>) {
        let [body, dock, message, input] = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        let title = match self.page {
            Page::Menu => "Welcome - command menu",
            page => COMMANDS.iter().find(|&&(p, _, _)| p == page).unwrap().2,
        };
        let block = || Block::bordered().title(format!(" eon watch | {title} "));
        let text = match self.page {
            Page::Menu => {
                let items = COMMANDS
                    .iter()
                    .map(|&(_, name, label)| format!("{name:<14} {label}"));
                let list = List::new(items)
                    .block(block())
                    .highlight_symbol("> ")
                    .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
                frame.render_stateful_widget(
                    list,
                    body,
                    &mut ListState::default().with_selected(Some(self.selected)),
                );
                None
            }
            Page::Status => Some(if let Some(error) = &self.error {
                format!("Replay failed: {error}")
            } else if self.status.is_none() {
                "Waiting for the journal".into()
            } else if self.recent.is_empty() {
                "No durable frames.".into()
            } else {
                self.recent
                    .iter()
                    .map(|(seq, len)| format!("seq {seq}: {len} payload bytes"))
                    .collect::<Vec<_>>()
                    .join("\n")
            }),
            Page::Capabilities => Some(format!(
                "Read-only local journal: {}\n/status: frame metadata\n/cyber: replay checksums and sequence\n/mesh: opt-in paired sync completions\n/audio: local opt-in input meter\n/vision: one explicitly selected still; no live capture",
                if self.error.is_some() {
                    "read failed"
                } else if self.status.is_some() {
                    "readable"
                } else {
                    "waiting"
                }
            )),
            Page::Cyber => Some(if let Some(error) = &self.error {
                format!("Replay failed: {error}")
            } else if let Some(status) = &self.status {
                if status.frames == 0 {
                    "No frames to check.".into()
                } else {
                    format!(
                        "Replay validated checksums and sequence for {} frame{}.\nOnly the local journal was checked.",
                        status.frames,
                        if status.frames == 1 { "" } else { "s" }
                    )
                }
            } else {
                "No replay result yet.".into()
            }),
            Page::Mesh => Some(if self.error.is_some() {
                "Journal replay failed; paired sync observations unavailable.".into()
            } else if let Some(syncs) = &self.syncs {
                format!(
                    "Completed paired syncs recorded: {}\nLast sync record at journal seq: {}",
                    syncs.count, syncs.last_seq
                )
            } else if self.status.is_some() {
                "No completed paired syncs recorded in this journal.".into()
            } else {
                "Waiting for the local journal.".into()
            }),
            Page::Vision => {
                let border = block();
                let inner = border.inner(body);
                frame.render_widget(border, body);
                if let vision::Selection::Frame(image) = &self.image {
                    let [label, canvas] =
                        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
                    frame.render_widget(Paragraph::new(self.image.status()), label);
                    if let Some(painter) = painter {
                        painter.draw(image, canvas, frame);
                    }
                } else {
                    frame.render_widget(
                        Paragraph::new(self.image.status()).wrap(Wrap { trim: false }),
                        inner,
                    );
                }
                None
            }
            Page::Audio => {
                let border = block();
                let inner = border.inner(body);
                frame.render_widget(border, body);
                let [status, graph] =
                    Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(inner);
                frame.render_widget(
                    Paragraph::new(self.meter.status()).wrap(Wrap { trim: false }),
                    status,
                );
                frame.render_widget(
                    Sparkline::default()
                        .data(self.meter.visible_peaks(graph.width))
                        .max(100),
                    graph,
                );
                None
            }
        };
        if let Some(text) = text {
            frame.render_widget(
                Paragraph::new(text)
                    .block(block())
                    .wrap(Wrap { trim: false }),
                body,
            );
        }
        frame.render_widget(Paragraph::new(self.dock()).reversed(), dock);
        let hint = if self.prompt.is_some() {
            "Tab/Shift-Tab: complete | Enter: open | Esc: cancel"
        } else if self.page == Page::Menu {
            "Up/Down: select | Enter: open"
        } else {
            "Esc: command menu"
        };
        frame.render_widget(
            Paragraph::new(self.feedback.as_deref().unwrap_or(hint)),
            message,
        );
        if let Some(prompt) = &self.prompt {
            // Prompt characters are ASCII; keep the insertion point visible on narrow terminals.
            let width = usize::from(input.width.saturating_sub(1));
            let shown = &prompt.text[prompt.text.len().saturating_sub(width)..];
            frame.render_widget(Paragraph::new(shown), input);
            if input.width > 0 && input.height > 0 {
                frame.set_cursor_position((input.x + shown.len() as u16, input.y));
            }
        } else {
            frame.render_widget(Paragraph::new("/: command | press q to quit"), input);
        }
    }
}

pub fn render_watch_snapshot(state: &WatchState, width: u16, height: u16) -> EonResult<String> {
    // Text snapshots never export protocol payloads or private pixels.
    crate::snapshot(width, height, |frame| state.draw(frame, None))
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(io::stdout(), DisableBracketedPaste);
        let _ = crossterm::execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
    }
}

/// Polls the journal on a timer independent of typing; keys only operate the local UI.
pub fn run_watch(dir: &Path, interval: Duration) -> EonResult<()> {
    run_watch_with_image(dir, interval, None)
}

/// Optionally load one operator-selected PNG or JPEG. The path is never retained.
pub fn run_watch_with_image(dir: &Path, interval: Duration, image: Option<&Path>) -> EonResult<()> {
    let io_err = |context: &str, e: io::Error| EonError::OperationFailed {
        operation: format!("watch.{context}"),
        reason: e.to_string(),
    };
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io_err(
            "terminal",
            io::Error::other("watch requires a terminal on stdin and stdout"),
        ));
    }

    let selection = image
        .map(vision::Selection::load)
        .transpose()
        .map_err(|reason| EonError::ValidationError(reason.into()))?
        .unwrap_or_default();
    crossterm::terminal::enable_raw_mode().map_err(|e| io_err("raw_mode", e))?;
    let _guard = TerminalGuard;
    crossterm::execute!(io::stdout(), EnterAlternateScreen)
        .map_err(|e| io_err("alternate_screen", e))?;
    crossterm::execute!(io::stdout(), EnableBracketedPaste).map_err(|e| io_err("paste", e))?;
    let mut terminal =
        Terminal::new(CrosstermBackend::new(io::stdout())).map_err(|e| io_err("terminal", e))?;
    let mut state = WatchState {
        image: selection,
        ..WatchState::default()
    };
    let mut painter = vision::Painter::new();
    state.refresh(dir);
    let mut refreshed = Instant::now();
    loop {
        terminal
            .draw(|frame| state.draw(frame, Some(&mut painter)))
            .map_err(|e| io_err("draw", e))?;
        let mut resized = false;
        if event::poll(interval.saturating_sub(refreshed.elapsed()))
            .map_err(|e| io_err("poll", e))?
        {
            let event = event::read().map_err(|e| io_err("read", e))?;
            resized = matches!(event, Event::Resize(..));
            state.handle_event(event);
        }
        if state.quit {
            return Ok(());
        }
        if (state.page != Page::Vision || resized)
            && painter
                .clear(io::stdout())
                .map_err(|e| io_err("image", e))?
        {
            terminal.clear().map_err(|e| io_err("clear", e))?;
        }
        if refreshed.elapsed() >= interval {
            state.meter.poll(Instant::now());
            state.refresh(dir);
            refreshed = Instant::now();
        }
    }
}
