//! Drive the real live pane with real files and a deterministic egui clock.
#![cfg(target_os = "linux")]

#[path = "support/pixels.rs"]
mod pixels;

use egui::{Color32, Context, Event, FullOutput, Pos2, RawInput, Rect, Shape, vec2};
use eon_canvas::view::LiveDiffView;
use std::{
    fs::{self, File},
    io::Write,
    os::unix::fs::{DirBuilderExt, symlink},
    path::{Path, PathBuf},
    sync::mpsc,
    time::Duration,
};

struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> Self {
        let dir =
            Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}", std::process::id()));
        fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        Self(dir)
    }

    fn file(&self, name: &str, body: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, body).unwrap();
        path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

const SIZE: [u32; 2] = [1160, 320];

fn paint(view: &mut LiveDiffView, ctx: &Context, time: f64, events: Vec<Event>) -> FullOutput {
    ctx.run_ui(
        RawInput {
            screen_rect: Some(Rect::from_min_size(
                Pos2::ZERO,
                vec2(SIZE[0] as f32, SIZE[1] as f32),
            )),
            time: Some(time),
            events,
            ..Default::default()
        },
        |ui| view.show(ui),
    )
}

fn frame(view: &mut LiveDiffView, ctx: &Context, time: f64, events: Vec<Event>) -> FullOutput {
    let mut output = paint(view, ctx, time, events);
    output.textures_delta.clear(); // Shape-only observation.
    output
}

fn labels(output: &FullOutput) -> Vec<&str> {
    output
        .shapes
        .iter()
        .filter_map(|s| match &s.shape {
            Shape::Text(t) => Some(t.galley.job.text.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn in_place_edits_recolor_actual_pixels_and_parse_failure_removes_them_until_recovery() {
    let f = Fixture::new("live-pixels");
    let source = b"def amount():\n    return 1\n";
    let before = f.file("private_before.py", source);
    let after = f.file("private_after.py", source);
    let mut view = LiveDiffView::open(&before, &after).unwrap();

    let mut counts = |time, error| {
        // A fresh painter uploads the full atlas for each readback; the live
        // view (including the open descriptors and poll deadline) is the same.
        let ctx = Context::default();
        let output = paint(&mut view, &ctx, time, vec![]);
        let labels = labels(&output).join("\n");
        let image = pixels::render(&ctx, output, SIZE);
        if error {
            assert!(labels.contains("After: source contains syntax errors. No diff shown."));
            for obsolete in ["private", "amount", "pending", "Apply"] {
                assert!(!labels.contains(obsolete));
            }
        } else {
            assert!(labels.contains("amount"));
        }
        let count = |color| image.pixels.iter().filter(|&&c| c == color).count();
        (
            count(Color32::from_rgb(90, 90, 100)),
            count(Color32::from_rgb(230, 179, 51)),
        )
    };
    let initial = counts(0.0, false);
    assert!(initial.0 > 25_000 && initial.1 == 0, "{initial:?}");
    // Same inode, same length, changed function body.
    fs::write(&after, b"def amount():\n    return 2\n").unwrap();
    assert_eq!(
        counts(0.25, false),
        initial,
        "a repaint must not bypass the poll interval"
    );
    let changed = counts(0.5, false);
    assert!(changed.0 == 0 && changed.1 > 25_000, "{changed:?}");
    fs::write(&after, b"def private_source(:\n").unwrap();
    assert_eq!(counts(1.0, true), (0, 0));
    assert_eq!(
        counts(1.5, true),
        (0, 0),
        "an unchanged error is not an empty valid diff"
    );
    fs::write(&after, source).unwrap();
    assert_eq!(counts(2.0, false), initial);
    eprintln!("unchanged/changed fill pixels: {initial:?} -> {changed:?} -> (0, 0) -> {initial:?}");
}

#[test]
fn identical_samples_keep_decisions_but_an_edit_to_either_input_starts_a_new_review() {
    let f = Fixture::new("live-decisions");
    let before = f.file("before.rs", b"fn amount() -> u32 { 1 }");
    let after = f.file("after.rs", b"fn amount() -> u32 { 2 }");
    let mut view = LiveDiffView::open(&before, &after).unwrap();
    let ctx = Context::default();
    let initial = frame(&mut view, &ctx, 0.0, vec![]);
    let pos = initial
        .shapes
        .iter()
        .find_map(|s| match &s.shape {
            Shape::Text(t) if t.galley.job.text == "NACK" => {
                Some(t.visual_bounding_rect().center())
            }
            _ => None,
        })
        .unwrap();
    let press = |pressed| Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: Default::default(),
    };
    frame(&mut view, &ctx, 0.05, vec![Event::PointerMoved(pos)]);
    for (time, pressed) in [(0.1, true), (0.15, false)] {
        frame(&mut view, &ctx, time, vec![press(pressed)]);
    }
    // Rewriting identical bytes may change metadata; it must not reset review.
    fs::write(&after, b"fn amount() -> u32 { 2 }").unwrap();
    assert!(labels(&frame(&mut view, &ctx, 0.5, vec![])).contains(&"Changed: Nacked"));
    frame(&mut view, &ctx, 0.75, vec![press(true)]);
    fs::write(&before, b"fn amount() -> u32 { 3 }").unwrap();
    assert!(labels(&frame(&mut view, &ctx, 1.0, vec![press(false)])).contains(&"Changed: Pending"));
    let idle = frame(&mut view, &ctx, 2.0, vec![]);
    let delay = idle.viewport_output[&egui::ViewportId::ROOT].repaint_delay;
    assert!(
        delay <= Duration::from_millis(500),
        "idle window never scheduled its next sample"
    );
}

#[test]
fn replacement_never_rebinds_and_input_limits_clear_the_review() {
    let f = Fixture::new("live-bounds");
    let before = f.file("before.rs", b"fn old() {}");
    let mut at_limit = b"fn held() {}".to_vec();
    at_limit.resize(64 * 1024, b' ');
    let after = f.file("after.rs", &at_limit);
    let mut writer = File::options().write(true).open(&after).unwrap();
    let mut view = LiveDiffView::open(&before, &after).unwrap();
    let ctx = Context::default();
    let mut at = |time| labels(&frame(&mut view, &ctx, time, vec![])).join("\n");
    assert!(at(0.0).contains("held"));
    fs::rename(f.file("replacement.rs", b"fn not_authorized() {}"), &after).unwrap();
    let pinned = at(0.5);
    assert!(pinned.contains("held") && !pinned.contains("not_authorized"));
    writer.set_len(64 * 1024 + 1).unwrap();
    let large = at(1.0);
    assert!(large.contains("After: source exceeds 64 KiB") && !large.contains("Review:"));
    writer.set_len(0).unwrap();
    writer.write_all(&[0xff]).unwrap();
    let invalid = at(1.5);
    assert!(invalid.contains("After: source is not UTF-8") && !invalid.contains("Apply"));
    writer.set_len(0).unwrap();
    assert!(
        at(2.0).contains("old"),
        "a valid empty input permits review again"
    );
    fs::write(&before, "fn x() {}\n".repeat(129)).unwrap();
    let complex = at(2.5);
    assert!(
        complex.contains("Before: source exceeds the AST work limit")
            && !complex.contains("Review:")
    );
    fs::write(&before, "fn restored() {}").unwrap();
    assert!(at(3.0).contains("restored"));
}

#[test]
fn acquisition_refuses_links_including_ancestors_directories_and_an_unopened_fifo() {
    let f = Fixture::new("private-acquisition");
    let regular = f.file("sample.py", b"");
    let link = f.0.join("link.rs");
    symlink(&regular, &link).unwrap();
    let parent_link = f.0.join("link-dir");
    symlink(&f.0, &parent_link).unwrap();
    let directory = f.0.join("directory.py");
    fs::create_dir(&directory).unwrap();
    let fifo = f.0.join("pipe.rs");
    rustix::fs::mkfifoat(rustix::fs::CWD, &fifo, rustix::fs::Mode::RUSR).unwrap();
    let (tx, rx) = mpsc::channel();
    let invalid = [
        fifo,
        link,
        parent_link.join("sample.py"),
        directory,
        f.0.join("missing.py"),
        f.file("other.txt", b""),
    ];
    std::thread::spawn(move || {
        for path in invalid {
            let err = LiveDiffView::open(&regular, &path)
                .err()
                .expect("unsafe input was accepted")
                .to_string();
            assert!(
                err.starts_with("After:")
                    && !err.contains("private")
                    && !err.contains(path.file_name().unwrap().to_str().unwrap())
            );
        }
        tx.send(()).unwrap();
    });
    rx.recv_timeout(Duration::from_secs(3))
        .expect("acquisition failed or blocked on a special file");
}
