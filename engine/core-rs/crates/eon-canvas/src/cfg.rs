//! Read-only view of the `;; N succs { … }` records from one GCC tree CFG dump.
//! Labels and directions come from the export; no source or instruction text is interpreted.

use std::{collections::BTreeMap, io::Read};

use egui::{Color32, Pos2, Stroke, Vec2, pos2, vec2};

use crate::render::{NODE_HEIGHT, NODE_WIDTH, node_centre};
use crate::view::{EDGE_STROKE, paint_node};

pub const MAX_BYTES: usize = 16 * 1024;
// At most 64 * 31 segments even after the layered layout inserts dummy nodes.
const MAX_BLOCKS: usize = 32;
const MAX_EDGES: usize = 64;
const RETURN_COLOR: Color32 = Color32::from_rgb(236, 110, 142);

type Successors = BTreeMap<u32, Vec<u32>>;

fn block_id(token: &str) -> Result<u32, &'static str> {
    if !token.bytes().all(|b| b.is_ascii_digit()) {
        return Err("expected a decimal block ID");
    }
    token.parse().map_err(|_| "block ID does not fit in u32")
}

fn read_graph(input: impl Read) -> Result<Successors, &'static str> {
    let mut text = String::new();
    input
        .take(MAX_BYTES as u64 + 1)
        .read_to_string(&mut text)
        .map_err(|_| "cannot read CFG records as UTF-8")?;
    if text.len() > MAX_BYTES {
        return Err("CFG exceeds 16 KiB");
    }
    let mut graph = Successors::new();
    let mut edges = 0;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let syntax = "expected one GCC successor record per line: ;; N succs { … }";
        let mut words = line
            .trim_end()
            .strip_suffix('}')
            .ok_or(syntax)?
            .split_ascii_whitespace();
        if words.next() != Some(";;") {
            return Err(syntax);
        }
        let id = block_id(words.next().ok_or(syntax)?)?;
        if words.next() != Some("succs") || words.next() != Some("{") {
            return Err(syntax);
        }
        let mut targets = Vec::new();
        for word in words {
            let target = block_id(word)?;
            edges += 1;
            if edges > MAX_EDGES {
                return Err("CFG exceeds 64 edges");
            }
            if targets.contains(&target) {
                return Err("duplicate successor");
            }
            targets.push(target);
        }
        if graph.insert(id, targets).is_some() {
            return Err("duplicate block; export only one function");
        }
        if graph.len() > MAX_BLOCKS {
            return Err("CFG exceeds 32 blocks");
        }
    }
    if graph.is_empty() {
        return Err("empty CFG");
    }
    // GCC references its artificial EXIT (1) without emitting a successor row for it.
    // Do not guess missing blocks, entries or edges from statement text.
    if graph.values().any(|targets| targets.contains(&1)) {
        graph.entry(1).or_default();
    }
    if graph.len() > MAX_BLOCKS {
        return Err("CFG exceeds 32 blocks");
    }
    if graph.values().flatten().any(|id| !graph.contains_key(id)) {
        return Err("successor has no block record");
    }
    Ok(graph)
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Visit {
    #[default]
    New,
    Active,
    Done,
}

// Remove only edges to the active DFS stack from the ranking input, not from the
// displayed CFG. Recursion depth is bounded by MAX_BLOCKS.
fn visit(
    u: usize,
    successors: &[Vec<usize>],
    state: &mut [Visit],
    down: &mut Vec<(usize, usize)>,
    returns: &mut Vec<(usize, usize)>,
) {
    state[u] = Visit::Active;
    for &v in &successors[u] {
        if state[v] == Visit::Active {
            returns.push((u, v));
        } else {
            down.push((u, v));
            if state[v] == Visit::New {
                visit(v, successors, state, down, returns);
            }
        }
    }
    state[u] = Visit::Done;
}

struct Block {
    centre: Pos2,
    label: String,
    successors: String,
}

struct Route {
    points: Vec<Pos2>,
    color: Color32,
}

/// One pre-laid-out graph. Scrolling moves the viewport, not a graph-sized GPU texture.
pub struct CfgView {
    blocks: Vec<Block>,
    routes: Vec<Route>,
    size: Vec2,
}

impl CfgView {
    pub fn read(input: impl Read) -> Result<Self, &'static str> {
        let graph = read_graph(input)?;
        let ids: Vec<_> = graph.keys().copied().collect();
        let successors: Vec<Vec<_>> = graph
            .values()
            .map(|targets| {
                targets
                    .iter()
                    .map(|id| ids.binary_search(id).unwrap())
                    .collect()
            })
            .collect();
        let mut state = vec![Visit::New; ids.len()];
        let (mut down, mut returns) = (Vec::new(), Vec::new());
        for i in 0..ids.len() {
            if state[i] == Visit::New {
                visit(i, &successors, &mut state, &mut down, &mut returns);
            }
        }
        let layout = crate::layered(ids.len(), &down).map_err(|_| "cannot rank CFG")?;
        let centre = |(rank, slot)| Pos2::from(node_centre(rank, slot));
        let blocks: Vec<_> = graph
            .iter()
            .enumerate()
            .map(|(i, (id, targets))| Block {
                centre: centre((layout.rank[i], layout.slot[i])),
                label: format!("bb {id}"),
                successors: if targets.is_empty() {
                    "No successors".into()
                } else {
                    format!(
                        "Successors: {}",
                        targets
                            .iter()
                            .map(u32::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                },
            })
            .collect();
        let right = blocks
            .iter()
            .map(|b| b.centre.x)
            .chain(layout.edge_paths.iter().flatten().map(|&p| centre(p).x))
            .fold(0.0, f32::max)
            + NODE_WIDTH / 2.0;
        let mut lane = right + 24.0;
        let half = NODE_HEIGHT / 2.0;
        let mut routes = Vec::new();
        for path in &layout.edge_paths {
            // Cross each rank boundary in the empty band between boxes. Inside an
            // intermediate rank the edge is vertical in its reserved dummy slot.
            let mut points = Vec::new();
            for hop in path.windows(2) {
                points.push(centre(hop[0]) + vec2(0.0, half));
                points.push(centre(hop[1]) - vec2(0.0, half));
            }
            routes.push(Route {
                points,
                color: EDGE_STROKE,
            });
        }
        for (u, v) in returns {
            let (a, b) = (blocks[u].centre, blocks[v].centre);
            let points = if u == v {
                let start = a + vec2(NODE_WIDTH / 2.0, 0.0);
                let end = a + vec2(NODE_WIDTH / 2.0 - 14.0, -half);
                vec![
                    start,
                    start + vec2(14.0, 0.0),
                    pos2(start.x + 14.0, end.y - 14.0),
                    end - vec2(0.0, 14.0),
                    end,
                ]
            } else {
                let start = a + vec2(NODE_WIDTH / 4.0, -half);
                let end = b + vec2(NODE_WIDTH / 4.0, half);
                let points = vec![
                    start,
                    start - vec2(0.0, 14.0),
                    pos2(lane, start.y - 14.0),
                    pos2(lane, end.y + 14.0),
                    end + vec2(0.0, 14.0),
                    end,
                ];
                lane += 14.0;
                points
            };
            routes.push(Route {
                points,
                color: RETURN_COLOR,
            });
        }
        let height = blocks.iter().map(|b| b.centre.y).fold(0.0, f32::max) + half + 32.0;
        Ok(Self {
            blocks,
            routes,
            size: vec2(lane + 16.0, height),
        })
    }

    pub fn show(&self, ui: &mut egui::Ui) {
        ui.label(format!(
            "{} blocks, {} directed edges. Hover a block for successors.",
            self.blocks.len(),
            self.routes.len()
        ));
        egui::ScrollArea::both().show(ui, |ui| {
            let origin = ui.next_widget_position().to_vec2();
            ui.set_min_size(self.size);
            for route in &self.routes {
                for pair in route.points.windows(2) {
                    ui.painter().line_segment(
                        [pair[0] + origin, pair[1] + origin],
                        Stroke::new(3.0, route.color),
                    );
                }
                let n = route.points.len();
                let tip = route.points[n - 1] + origin;
                let direction = (tip - (route.points[n - 2] + origin)).normalized();
                let base = tip - direction * 11.0;
                let wing = direction.rot90() * 5.0;
                ui.painter().add(egui::Shape::convex_polygon(
                    vec![tip, base + wing, base - wing],
                    route.color,
                    Stroke::NONE,
                ));
            }
            for block in &self.blocks {
                let rect = paint_node(ui.painter(), block.centre + origin, &block.label);
                if ui.is_rect_visible(rect) {
                    ui.interact(rect, ui.scope_id().with(&block.label), egui::Sense::hover())
                        .on_hover_text(&block.successors);
                }
            }
        });
    }
}

impl eframe::App for CfgView {
    fn ui(&mut self, ui: &mut egui::Ui, _: &mut eframe::Frame) {
        self.show(ui);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_or_mixed_records_are_not_partially_displayed() {
        for bad in [
            "",
            ";; 2 succs { 9 }",
            ";; 2 succs { 1 1 }",
            ";; 2 succs { 1 }\n;; 2 succs { 1 }",
            ";; 2 succs { -2 }",
            ";; 2 succs { 1 } trailing",
            ";; 2 succs { 1 }\nsource text",
            ";; 4294967296 succs { }",
            ";; 0 succs {",
            ";; 0 fails { }",
        ] {
            assert!(CfgView::read(bad.as_bytes()).is_err(), "{bad:?}");
        }
        assert!(CfgView::read(&[0xff][..]).is_err());
        assert!(CfgView::read(std::io::repeat(b' ')).is_err());
    }

    #[test]
    fn budgets_include_implicit_exit_and_arent_indexed_by_block_id() {
        let sparse =
            CfgView::read(&b";; 4294967295 succs { 4294967295 }\n;; 12 succs { }"[..]).unwrap();
        assert_eq!((sparse.blocks.len(), sparse.routes.len()), (2, 1));
        let rows = |count: usize| {
            (2..2 + count)
                .map(|id| format!(";; {id} succs {{ 1 }}\n"))
                .collect::<String>()
        };
        assert_eq!(
            CfgView::read(rows(MAX_BLOCKS - 1).as_bytes())
                .unwrap()
                .blocks
                .len(),
            MAX_BLOCKS
        );
        assert!(CfgView::read(rows(MAX_BLOCKS).as_bytes()).is_err());
        let mut dense = String::new();
        for id in 2..10 {
            dense += &format!(";; {id} succs {{ 2 3 4 5 6 7 8 9 }}\n");
        }
        assert_eq!(
            CfgView::read(dense.as_bytes()).unwrap().routes.len(),
            MAX_EDGES
        );
        dense += ";; 10 succs { 2 }";
        assert!(CfgView::read(dense.as_bytes()).is_err());
    }
}
