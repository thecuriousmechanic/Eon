//! egui view of a layout. Boxes, edge segments, and node labels are painted with egui's painter, using the same geometry as
//! the offscreen renderer. [`LayoutView`] is the window shell, run with `eframe`.

use eframe::egui;
use eon_cyber::cpg::FunctionBlock;
use std::sync::Arc;

use crate::Layout;
use crate::diff::{Decision, Pairing, Review, Status};
use crate::render::{NODE_HEIGHT, NODE_WIDTH, node_centre};

#[cfg(target_os = "linux")]
mod live;
#[cfg(target_os = "linux")]
pub use live::{LiveDiffView, run_live_diff};

const NODE_FILL: egui::Color32 = egui::Color32::from_rgb(51, 128, 230);
pub(crate) const EDGE_STROKE: egui::Color32 = egui::Color32::from_rgb(230, 179, 51);
const LABEL: egui::Color32 = egui::Color32::WHITE;
const BLOCK_WIDTH: f32 = 360.0;
const BLOCK_HEIGHT: f32 = 44.0;
const ROW_HEIGHT: f32 = 56.0;
const NEW_COLUMN_X: f32 = 460.0;
const RIBBON: egui::Color32 = egui::Color32::from_rgba_premultiplied(120, 120, 130, 150);

fn status_fill(status: Status) -> egui::Color32 {
    match status {
        Status::Unchanged => egui::Color32::from_rgb(90, 90, 100),
        Status::Changed => egui::Color32::from_rgb(230, 179, 51),
        Status::Added => egui::Color32::from_rgb(80, 190, 120),
        Status::Removed => egui::Color32::from_rgb(220, 80, 80),
    }
}

fn block_rect(origin: egui::Pos2, x: f32, row: usize) -> egui::Rect {
    let top_left = origin + egui::vec2(x, 40.0 + row as f32 * ROW_HEIGHT);
    egui::Rect::from_min_size(top_left, egui::vec2(BLOCK_WIDTH, BLOCK_HEIGHT))
}

/// Paints the structural diff: old functions on the left, new on the right, and a ribbon between each matched pair.
pub fn paint_diff(
    painter: &egui::Painter,
    origin: egui::Pos2,
    old: &[FunctionBlock],
    new: &[FunctionBlock],
    pairings: &[Pairing],
) {
    for pairing in pairings {
        let old_rect = pairing.old.map(|i| block_rect(origin, 40.0, i));
        let new_rect = pairing.new.map(|j| block_rect(origin, NEW_COLUMN_X, j));
        if let (Some(from), Some(to)) = (old_rect, new_rect) {
            let start = from.right_center();
            let end = to.left_center();
            let reach = (end.x - start.x) / 2.0;
            let points = [
                start,
                start + egui::vec2(reach, 0.0),
                end - egui::vec2(reach, 0.0),
                end,
            ];
            painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
                points,
                false,
                egui::Color32::TRANSPARENT,
                egui::Stroke::new(1.5, RIBBON),
            ));
        }
        for (rect, index, blocks) in [(old_rect, pairing.old, old), (new_rect, pairing.new, new)] {
            if let (Some(rect), Some(index)) = (rect, index) {
                painter.rect_filled(rect, 4.0, status_fill(pairing.status));
                painter.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    &blocks[index].name,
                    egui::FontId::monospace(13.0),
                    LABEL,
                );
            }
        }
    }
}

const CONTROL_LABEL_X: f32 = 840.0;
const APPLY_X: f32 = 1000.0;
const NACK_X: f32 = 1070.0;
const CONTROL_WIDTH: f32 = 64.0;
const CONTROL_HEIGHT: f32 = 20.0;

/// Where the Apply (`nack` false) or NACK (`nack` true) button of review row `row` sits.
pub fn control_rect(origin: egui::Pos2, row: usize, nack: bool) -> egui::Rect {
    let x = if nack { NACK_X } else { APPLY_X };
    let top_left = origin + egui::vec2(x, 40.0 + row as f32 * ROW_HEIGHT + 12.0);
    egui::Rect::from_min_size(top_left, egui::vec2(CONTROL_WIDTH, CONTROL_HEIGHT))
}

/// Apply and NACK buttons for every pairing that needs review, one row per pairing in list order. A click records the
/// decision in `review`.
pub fn review_controls(
    ui: &mut egui::Ui,
    origin: egui::Pos2,
    old: &[FunctionBlock],
    new: &[FunctionBlock],
    pairings: &[Pairing],
    review: &mut Review,
) {
    for (row, pairing) in pairings.iter().enumerate() {
        if pairing.status == Status::Unchanged {
            continue;
        }
        let name = pairing
            .new
            .map(|j| new[j].name.as_str())
            .or_else(|| pairing.old.map(|i| old[i].name.as_str()))
            .unwrap_or_default();
        let rect = block_rect(origin, CONTROL_LABEL_X, row);
        let label_clip =
            egui::Rect::from_min_max(rect.min, egui::pos2(origin.x + APPLY_X - 4.0, rect.max.y));
        let painter = ui
            .painter()
            .with_clip_rect(ui.clip_rect().intersect(label_clip));
        painter.text(
            rect.left_top(),
            egui::Align2::LEFT_TOP,
            name,
            egui::FontId::monospace(12.0),
            LABEL,
        );
        if ui
            .put(control_rect(origin, row, false), egui::Button::new("Apply"))
            .clicked()
        {
            review.decide(row, Decision::Applied);
        }
        if ui
            .put(control_rect(origin, row, true), egui::Button::new("NACK"))
            .clicked()
        {
            review.decide(row, Decision::Nacked);
        }
        if let Some(decision) = review.decision(row) {
            painter.text(
                rect.left_center(),
                egui::Align2::LEFT_CENTER,
                format!("{:?}: {decision:?}", pairing.status),
                egui::FontId::monospace(12.0),
                LABEL,
            );
        }
    }
}

pub(crate) fn paint_node(painter: &egui::Painter, centre: egui::Pos2, label: &str) -> egui::Rect {
    let rect = egui::Rect::from_center_size(centre, egui::vec2(NODE_WIDTH, NODE_HEIGHT));
    painter.rect_filled(rect, 4.0, NODE_FILL);
    painter.text(
        centre,
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(14.0),
        LABEL,
    );
    rect
}

pub fn paint_layout(painter: &egui::Painter, origin: egui::Pos2, layout: &Layout) {
    let at = |p: [f32; 2]| origin + egui::vec2(p[0], p[1]);
    for path in &layout.edge_paths {
        for pair in path.windows(2) {
            let a = node_centre(pair[0].0, pair[0].1);
            let b = node_centre(pair[1].0, pair[1].1);
            painter.line_segment([at(a), at(b)], egui::Stroke::new(3.0, EDGE_STROKE));
        }
    }
    for (index, (&rank, &slot)) in layout.rank.iter().zip(&layout.slot).enumerate() {
        paint_node(painter, at(node_centre(rank, slot)), &format!("n{index}"));
    }
}

pub struct LayoutView {
    layout: Layout,
}

impl LayoutView {
    pub fn new(layout: Layout) -> Self {
        Self { layout }
    }
}

impl eframe::App for LayoutView {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let origin = ui.max_rect().min;
        paint_layout(ui.painter(), origin, &self.layout);
    }
}

/// Opens a window that shows `layout` until it is closed.
pub fn run(layout: Layout) -> eframe::Result {
    eframe::run_native(
        "eon-canvas",
        eframe::NativeOptions::default(),
        Box::new(move |_creation| Ok(Box::new(LayoutView::new(layout)))),
    )
}

pub struct DiffView {
    old: Arc<[FunctionBlock]>,
    new: Arc<[FunctionBlock]>,
    pairings: Vec<Pairing>,
    review: Review,
}

impl DiffView {
    pub fn new(old: Vec<FunctionBlock>, new: Vec<FunctionBlock>) -> Self {
        Self::from_blocks(old.into(), new.into())
    }

    fn from_blocks(old: Arc<[FunctionBlock]>, new: Arc<[FunctionBlock]>) -> Self {
        let pairings = crate::diff::match_blocks(&old, &new);
        let review = Review::new(&pairings);
        Self {
            old,
            new,
            pairings,
            review,
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui) {
        let pending = self.review.pending();
        ui.label(format!(
            "Review: {pending} pending | in-memory decisions; no files written"
        ));
        if self.pairings.is_empty() {
            ui.label("No function blocks in either source.");
            return;
        }
        ui.label("Before (left) / after (right). Hover a block to inspect its source.");
        egui::ScrollArea::both().show(ui, |ui| {
            let origin = ui.next_widget_position();
            let rows = self.pairings.len();
            ui.set_min_size(egui::vec2(
                NACK_X + CONTROL_WIDTH + 8.0,
                40.0 + rows as f32 * ROW_HEIGHT,
            ));
            paint_diff(ui.painter(), origin, &self.old, &self.new, &self.pairings);
            for (side, x, blocks) in [
                ("Before", 40.0, &self.old),
                ("After", NEW_COLUMN_X, &self.new),
            ] {
                for (i, block) in blocks.iter().enumerate() {
                    let rect = block_rect(origin, x, i);
                    if ui.is_rect_visible(rect) {
                        ui.interact(rect, ui.scope_id().with((side, i)), egui::Sense::hover())
                            .on_hover_ui(|ui| {
                                ui.label(format!(
                                    "{side}: {} (line {})",
                                    block.name, block.start_line
                                ));
                                ui.monospace(&block.text);
                            });
                    }
                }
            }
            review_controls(
                ui,
                origin,
                &self.old,
                &self.new,
                &self.pairings,
                &mut self.review,
            );
        });
        if self.review.pending() != pending {
            ui.ctx().request_repaint();
        }
    }
}

impl eframe::App for DiffView {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
    }
}

fn diff_window_options() -> eframe::NativeOptions {
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1160.0, 400.0]),
        ..Default::default()
    }
}

/// Opens a window that shows the structural diff of two sources until it is closed.
pub fn run_diff(old: Vec<FunctionBlock>, new: Vec<FunctionBlock>) -> eframe::Result {
    eframe::run_native(
        "eon-canvas diff",
        diff_window_options(),
        Box::new(move |_creation| Ok(Box::new(DiffView::new(old, new)))),
    )
}
