//! Linux live diffs follow two file descriptions, never their original names.

use super::DiffView;
use eon_cyber::cpg::{FunctionBlock, Lang};
use rustix::fs::{CWD, Mode, OFlags, ResolveFlags, openat2};
use std::{
    fs::File,
    io::{self, Read, Seek},
    ops::ControlFlow,
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    path::Path,
    sync::Arc,
};
use tree_sitter::{ParseOptions, Parser};

const POLL_SECONDS: f64 = 0.5;
const MAX_BYTES: usize = 64 * 1024;
const MAX_PARSE_CHECKS: usize = 1024;
const MAX_NODES: usize = 24_000;
const MAX_FUNCTIONS: usize = 128;
const MAX_BLOCK_BYTES: usize = 256 * 1024;
const WORK_LIMIT: &str = "source exceeds the AST work limit";

type Sample<T> = Result<T, &'static str>;

struct Source {
    file: File,
    lang: Lang,
    bytes: Option<Vec<u8>>,
    blocks: Sample<Arc<[FunctionBlock]>>,
}

impl Source {
    fn open(path: &Path) -> Sample<Self> {
        let lang = match path.extension().and_then(|ext| ext.to_str()) {
            Some("rs") => Lang::Rust,
            Some("py") => Lang::Python,
            _ => return Err("expected a .rs or .py source file"),
        };
        // O_PATH doesn't invoke device/FIFO open handlers. NO_SYMLINKS covers
        // ancestors as well as the leaf, in the same resolution as the pin.
        let pin: File = openat2(
            CWD,
            path,
            OFlags::PATH | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::NO_SYMLINKS,
        )
        .map_err(|_| "cannot acquire source without following links")?
        .into();
        if !pin
            .metadata()
            .map_err(|_| "cannot inspect source")?
            .is_file()
        {
            return Err("expected a regular source file");
        }
        // This kernel magic link refers to the checked, still-open descriptor.
        // It is used once for the read-only open, never to re-resolve `path`.
        let file = File::open(format!("/proc/self/fd/{}", pin.as_raw_fd()))
            .map_err(|_| "cannot open pinned source for reading")?;
        Ok(Self {
            file,
            lang,
            bytes: None,
            blocks: Err("not sampled"),
        })
    }

    fn read(&mut self) -> Sample<Vec<u8>> {
        const READ_ERROR: &str = "could not read opened file";
        const SIZE_LIMIT: &str = "source exceeds 64 KiB";
        let before = self.file.metadata().map_err(|_| READ_ERROR)?;
        if before.len() > MAX_BYTES as u64 {
            return Err(SIZE_LIMIT);
        }
        self.file.rewind().map_err(|_| READ_ERROR)?;
        let mut bytes = Vec::with_capacity(before.len() as usize);
        self.file
            .by_ref()
            .take(MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| READ_ERROR)?;
        if bytes.len() > MAX_BYTES {
            return Err(SIZE_LIMIT);
        }
        let after = self.file.metadata().map_err(|_| READ_ERROR)?;
        let stamp = |m: &std::fs::Metadata| {
            (
                m.len(),
                m.mtime(),
                m.mtime_nsec(),
                m.ctime(),
                m.ctime_nsec(),
            )
        };
        if stamp(&before) != stamp(&after) || after.len() != bytes.len() as u64 {
            return Err("source changed during read");
        }
        Ok(bytes)
    }

    /// Cache both successes and parse failures by exact bytes, not timestamps.
    fn poll(&mut self) -> bool {
        match self.read() {
            Ok(bytes) if self.bytes.as_ref() == Some(&bytes) => false,
            Ok(bytes) => {
                self.blocks = parse_blocks(self.lang, &bytes).map(Into::into);
                self.bytes = Some(bytes);
                true
            }
            Err(reason) => {
                self.bytes = None;
                self.blocks = Err(reason);
                true
            }
        }
    }
}

// The static inspection API accepts recovered trees and recursively collects
// every block. Live input can be mid-edit: reject recovery, and bound work
// *before* copying nested block text or invoking the quadratic name matcher.
fn parse_blocks(lang: Lang, bytes: &[u8]) -> Sample<Vec<FunctionBlock>> {
    let text = std::str::from_utf8(bytes).map_err(|_| "source is not UTF-8")?;
    let (grammar, function_kind) = match lang {
        Lang::Rust => (tree_sitter_rust::LANGUAGE, "function_item"),
        Lang::Python => (tree_sitter_python::LANGUAGE, "function_definition"),
    };
    let mut parser = Parser::new();
    parser
        .set_language(&grammar.into())
        .map_err(|_| "parser unavailable")?;
    let mut checks = 0;
    let mut budget = |_: &tree_sitter::ParseState| {
        checks += 1;
        if checks >= MAX_PARSE_CHECKS {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let tree = parser
        .parse_with_options(
            &mut |offset, _| bytes.get(offset..).unwrap_or_default(),
            None,
            Some(ParseOptions::new().progress_callback(&mut budget)),
        )
        .ok_or(WORK_LIMIT)?;
    let root = tree.root_node();
    if root.has_error() {
        return Err("source contains syntax errors");
    }
    if root.descendant_count() > MAX_NODES {
        return Err(WORK_LIMIT);
    }
    let mut cursor = root.walk();
    let mut blocks = Vec::new();
    let mut block_bytes = 0;
    loop {
        let node = cursor.node();
        if node.kind() == function_kind {
            let name = node
                .child_by_field_name("name")
                .ok_or("function has no name")?;
            block_bytes += node.byte_range().len();
            if blocks.len() == MAX_FUNCTIONS || block_bytes > MAX_BLOCK_BYTES {
                return Err(WORK_LIMIT);
            }
            blocks.push(FunctionBlock {
                name: text[name.byte_range()].into(),
                start_line: node.start_position().row + 1,
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
                text: text[node.byte_range()].into(),
            });
        }
        if !cursor.goto_first_child() {
            while !cursor.goto_next_sibling() {
                if !cursor.goto_parent() {
                    return Ok(blocks);
                }
            }
        }
    }
}

/// Polls only the two opened files. In-place changes are visible; renaming or
/// replacing a pathname never rebinds the viewer. Reopen to select new inodes.
pub struct LiveDiffView {
    before: Source,
    after: Source,
    next_poll: f64,
    generation: u64,
    view: Option<DiffView>,
}

impl LiveDiffView {
    /// Acquire exactly two regular files without following symlinks. Requires
    /// Linux openat2 and a trusted procfs; there is no ambient-path fallback.
    pub fn open(before: &Path, after: &Path) -> io::Result<Self> {
        Ok(Self {
            before: Source::open(before)
                .map_err(|reason| io::Error::other(format!("Before: {reason}")))?,
            after: Source::open(after)
                .map_err(|reason| io::Error::other(format!("After: {reason}")))?,
            next_poll: f64::NEG_INFINITY,
            generation: 0,
            view: None,
        })
    }

    pub fn show(&mut self, ui: &mut egui::Ui) {
        let now = ui.input(|input| input.time);
        if now >= self.next_poll {
            self.next_poll = now + POLL_SECONDS;
            let before_changed = self.before.poll();
            let after_changed = self.after.poll();
            if before_changed || after_changed {
                self.generation = self.generation.wrapping_add(1);
                self.view = match (&self.before.blocks, &self.after.blocks) {
                    (Ok(old), Ok(new)) => Some(DiffView::from_blocks(old.clone(), new.clone())),
                    _ => None,
                };
            }
        }
        ui.ctx()
            .request_repaint_after_secs((self.next_poll - now) as f32);
        ui.label("Live: polling the opened files for in-place edits. Reopen after an atomic save.");
        if let Some(view) = &mut self.view {
            // A press on an old row must not become a release on a new one.
            ui.push_id(self.generation, |ui| view.show(ui));
        } else {
            for (side, source) in [("Before", &self.before), ("After", &self.after)] {
                if let Err(reason) = source.blocks {
                    ui.label(format!("{side}: {reason}. No diff shown."));
                }
            }
        }
    }
}

impl eframe::App for LiveDiffView {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
    }
}

/// Show an already authorized file pair. Dropping the view closes both handles.
pub fn run_live_diff(view: LiveDiffView) -> eframe::Result {
    eframe::run_native(
        "eon-canvas live diff",
        super::diff_window_options(),
        Box::new(move |_| Ok(Box::new(view))),
    )
}
