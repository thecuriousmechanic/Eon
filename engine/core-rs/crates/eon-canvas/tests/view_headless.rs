//! The egui view paints one box and one label per node, and one line per edge segment. Runs without a window.

use egui::{Event, Modifiers, PointerButton, Pos2, RawInput, Rect, Shape, vec2};
use eon_canvas::diff::{Decision, Review, match_blocks};
use eon_canvas::layered;
use eon_canvas::view::{DiffView, control_rect, paint_diff, paint_layout, review_controls};
use eon_cyber::cpg::{Lang, function_blocks};

#[test]
fn the_view_paints_a_box_and_label_per_node_and_a_segment_per_edge_hop() {
    // The edge 0 -> 2 spans two ranks, so it becomes two segments through a dummy node.
    let layout = layered(3, &[(0, 1), (0, 2), (1, 2)]).unwrap();
    let ctx = egui::Context::default();
    let mut output = ctx.run_ui(RawInput::default(), |ui| {
        paint_layout(ui.painter(), egui::pos2(0.0, 0.0), &layout);
    });
    output.textures_delta.clear();

    let shapes: Vec<&Shape> = output.shapes.iter().map(|clipped| &clipped.shape).collect();
    let boxes = shapes
        .iter()
        .filter(|s| matches!(s, Shape::Rect(_)))
        .count();
    let labels = shapes
        .iter()
        .filter(|s| matches!(s, Shape::Text(_)))
        .count();
    let segments = shapes
        .iter()
        .filter(|s| matches!(s, Shape::LineSegment { .. }))
        .count();

    assert_eq!(boxes, 3);
    assert_eq!(labels, 3);
    assert_eq!(segments, 4);
}

#[test]
fn the_diff_paints_two_blocks_and_one_ribbon_per_matched_pair() {
    let old = function_blocks(Lang::Rust, "fn a() { 1 }\nfn b() { 2 }\nfn gone() {}\n").unwrap();
    let new = function_blocks(Lang::Rust, "fn a() { 1 }\nfn b() { 3 }\nfn fresh() {}\n").unwrap();
    let pairings = match_blocks(&old, &new);
    let ctx = egui::Context::default();
    let mut output = ctx.run_ui(RawInput::default(), |ui| {
        paint_diff(ui.painter(), egui::pos2(0.0, 0.0), &old, &new, &pairings);
    });
    output.textures_delta.clear();

    let shapes: Vec<&Shape> = output.shapes.iter().map(|clipped| &clipped.shape).collect();
    let boxes = shapes
        .iter()
        .filter(|s| matches!(s, Shape::Rect(_)))
        .count();
    let ribbons = shapes
        .iter()
        .filter(|s| matches!(s, Shape::CubicBezier(_)))
        .count();

    // a and b are matched (two boxes each), gone is removed and fresh is added (one box each).
    assert_eq!(boxes, 6);
    assert_eq!(ribbons, 2);
}

fn review_frame(
    ctx: &egui::Context,
    events: Vec<Event>,
    old: &[eon_cyber::cpg::FunctionBlock],
    new: &[eon_cyber::cpg::FunctionBlock],
    pairings: &[eon_canvas::diff::Pairing],
    review: &mut Review,
) {
    let input = RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1200.0, 800.0))),
        events,
        ..RawInput::default()
    };
    let mut output = ctx.run_ui(input, |ui| {
        review_controls(ui, Pos2::ZERO, old, new, pairings, review)
    });
    output.textures_delta.clear();
}

fn click(
    ctx: &egui::Context,
    at: Pos2,
    old: &[eon_cyber::cpg::FunctionBlock],
    new: &[eon_cyber::cpg::FunctionBlock],
    pairings: &[eon_canvas::diff::Pairing],
    review: &mut Review,
) {
    review_frame(ctx, vec![], old, new, pairings, review);
    review_frame(
        ctx,
        vec![Event::PointerMoved(at)],
        old,
        new,
        pairings,
        review,
    );
    let press = |pressed| Event::PointerButton {
        pos: at,
        button: PointerButton::Primary,
        pressed,
        modifiers: Modifiers::NONE,
    };
    review_frame(ctx, vec![press(true)], old, new, pairings, review);
    review_frame(ctx, vec![press(false)], old, new, pairings, review);
}

#[test]
fn apply_and_nack_buttons_record_the_reviewer_decision() {
    let old = function_blocks(Lang::Rust, "fn a() { 1 }\nfn b() { 2 }\n").unwrap();
    let new = function_blocks(Lang::Rust, "fn a() { 3 }\nfn b() { 4 }\n").unwrap();
    let pairings = match_blocks(&old, &new);
    let mut review = Review::new(&pairings);
    let ctx = egui::Context::default();

    click(
        &ctx,
        control_rect(Pos2::ZERO, 0, false).center(),
        &old,
        &new,
        &pairings,
        &mut review,
    );
    click(
        &ctx,
        control_rect(Pos2::ZERO, 1, true).center(),
        &old,
        &new,
        &pairings,
        &mut review,
    );

    assert_eq!(review.decision(0), Some(Decision::Applied));
    assert_eq!(review.decision(1), Some(Decision::Nacked));
    assert_eq!(review.pending(), 0);
}

#[test]
fn window_shows_source_and_updates_pending_count_and_row_decision_after_clicks() {
    let old = function_blocks(Lang::Rust, "fn price() -> u8 { 1 }").unwrap();
    let new = function_blocks(Lang::Rust, "fn price() -> u8 { 2 }").unwrap();
    let mut view = DiffView::new(old, new);
    let ctx = egui::Context::default();
    let mut time = 0.0;
    let mut frame = |events| {
        time += 0.05;
        let mut output = ctx.run_ui(
            RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1200.0, 400.0))),
                events,
                time: Some(time),
                ..Default::default()
            },
            |ui| view.show(ui),
        );
        output.textures_delta.clear();
        output
            .shapes
            .into_iter()
            .filter_map(|s| match s.shape {
                Shape::Text(t) => {
                    Some((t.galley.job.text.clone(), t.visual_bounding_rect().center()))
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    frame(vec![]);
    let labels = frame(vec![]);
    let center = |label| labels.iter().find(|(text, _)| text == label).unwrap().1;
    assert!(labels.iter().any(|(s, _)| s.contains("1 pending")));
    let at = center("price");
    frame(vec![Event::PointerMoved(at)]);
    for _ in 0..20 {
        frame(vec![]);
    }
    let hover = frame(vec![]);
    assert!(hover.iter().any(|(s, _)| s == "fn price() -> u8 { 1 }"));

    for (button, decision) in [("Apply", "Applied"), ("NACK", "Nacked")] {
        let pos = center(button);
        frame(vec![Event::PointerMoved(pos)]);
        for pressed in [true, false] {
            frame(vec![Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed,
                modifiers: Modifiers::NONE,
            }]);
        }
        let labels = frame(vec![]);
        assert!(
            labels
                .iter()
                .any(|(s, _)| s == &format!("Changed: {decision}")),
            "labels after {button}: {labels:?}"
        );
        assert!(
            labels
                .iter()
                .any(|(s, _)| s.contains("0 pending") && s.contains("no files written"))
        );
    }
}

#[test]
fn narrow_window_can_scroll_to_the_last_review_controls() {
    let source = |value| {
        (0..15)
            .map(|i| format!("fn row_{i}() -> u8 {{ {value} }}\n"))
            .collect::<String>()
    };
    let old = function_blocks(Lang::Rust, &source(1)).unwrap();
    let new = function_blocks(Lang::Rust, &source(2)).unwrap();
    let mut view = DiffView::new(old, new);
    let ctx = egui::Context::default();
    let mut frame = |events| {
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(500.0, 180.0));
        let mut output = ctx.run_ui(
            RawInput {
                screen_rect: Some(screen),
                events,
                ..Default::default()
            },
            |ui| view.show(ui),
        );
        output.textures_delta.clear();
        output
            .shapes
            .into_iter()
            .filter_map(|s| match s.shape {
                Shape::Text(t)
                    if screen
                        .intersect(s.clip_rect)
                        .contains(t.visual_bounding_rect().center()) =>
                {
                    Some(t.galley.job.text.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let labels = frame(vec![]);
    assert!(!labels.iter().any(|s| s == "row_14" || s == "NACK"));
    let at = egui::pos2(230.0, 130.0);
    frame(vec![Event::PointerMoved(at)]);
    frame(vec![Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: vec2(-1600.0, -1800.0),
        modifiers: Modifiers::NONE,
        phase: egui::TouchPhase::Move,
        source: egui::MouseWheelSource::Trackpad,
    }]);
    for _ in 0..60 {
        let labels = frame(vec![]);
        if labels.iter().any(|s| s == "row_14") && labels.iter().any(|s| s == "NACK") {
            return;
        }
    }
    panic!("last row and its controls were not reachable");
}
