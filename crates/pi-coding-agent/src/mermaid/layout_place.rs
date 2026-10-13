//! Node placement and edge track planning for top-down and left-to-right layouts
//! (upstream `layout.ts`).

use super::graph::Graph;
use super::labels::MAX_LABEL;
use super::layout_rank::{Span5, assign_positions, assign_tracks};
use super::width::string_width;

/// Minimum horizontal and vertical space between boxes.
const GAP_X: usize = 3;
const GAP_Y: usize = 2;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Placed {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
    pub cx: usize,
    pub cy: usize,
    pub rank: usize,
}

/// Per-node dimensions. `lay_*` include room for self-edge loops and labels.
pub(super) struct NodeSizes {
    pub box_w: Vec<usize>,
    pub box_h: Vec<usize>,
    pub lay_w: Vec<usize>,
    pub lay_h: Vec<usize>,
    pub extra_h: Vec<usize>,
    pub self_label_w: Vec<usize>,
}

pub(super) struct RoutePlan {
    pub canvas_w: usize,
    pub canvas_h: usize,
    /// Coordinate just past each rank's boxes, where its bus rows begin.
    pub band_end: Vec<usize>,
    /// Bus track offset per edge.
    pub edge_bus: Vec<usize>,
    /// Coordinate of the first lane track.
    pub lane_base: usize,
    /// Lane track offset per edge.
    pub edge_lane: Vec<usize>,
}

/// Edges from rank `r` to `r + 1` that must jog sideways, so need a bus row.
fn bus_spans(graph: &Graph, ranks: &[usize], centers: &[usize], r: usize, exact: bool) -> Vec<Span5> {
    let mut out = Vec::new();
    for (i, e) in graph.edges.iter().enumerate() {
        let (a, b) = (centers[e.from], centers[e.to]);
        let jogs = if exact { a != b } else { a.abs_diff(b) > 1 };
        if e.from != e.to && ranks[e.from] == r && ranks[e.to] == r + 1 && jogs {
            out.push([a.min(b), a.max(b), e.from, e.to, i]);
        }
    }
    out
}

/// Edges skipping a rank or running backwards; these go around in a lane.
fn lane_spans(graph: &Graph, ranks: &[usize], placed: &[Placed], vertical: bool) -> Vec<Span5> {
    let mut out = Vec::new();
    for (i, e) in graph.edges.iter().enumerate() {
        if e.from == e.to || ranks[e.to] == ranks[e.from] + 1 {
            continue;
        }
        let (pf, pt) = (placed[e.from], placed[e.to]);
        let (a, b) = if vertical { (pf.cy, pt.cy) } else { (pf.cx, pt.cx) };
        out.push([a.min(b), a.max(b), e.from, e.to, i]);
    }
    out
}

/// Bus track per edge between each pair of ranks, and the track count per rank.
fn plan_buses(
    graph: &Graph,
    ranks: &[usize],
    centers: &[usize],
    max_rank: usize,
    exact: bool,
) -> (Vec<usize>, Vec<usize>) {
    let mut edge_bus = vec![0; graph.edges.len()];
    let mut bus_tracks = vec![0; max_rank + 1];
    for (r, tracks) in bus_tracks.iter_mut().enumerate().take(max_rank) {
        let spans = bus_spans(graph, ranks, centers, r, exact);
        if spans.is_empty() {
            continue;
        }
        let (assigned, count) = assign_tracks(&spans);
        for (idx, slot) in assigned {
            edge_bus[idx] = slot;
        }
        *tracks = count;
    }
    (edge_bus, bus_tracks)
}

/// Lane track per edge plus the lane count, or zero lanes when no edge needs one.
fn plan_lanes(graph: &Graph, ranks: &[usize], placed: &[Placed], vertical: bool) -> (Vec<usize>, usize) {
    let mut edge_lane = vec![0; graph.edges.len()];
    let lanes = lane_spans(graph, ranks, placed, vertical);
    if lanes.is_empty() {
        return (edge_lane, 0);
    }
    let (assigned, count) = assign_tracks(&lanes);
    for (idx, slot) in assigned {
        edge_lane[idx] = slot;
    }
    (edge_lane, count)
}

pub(super) fn place_td(
    ranks: &[usize],
    max_rank: usize,
    by_rank: &[Vec<usize>],
    sizes: &NodeSizes,
    graph: &Graph,
    placed: &mut [Placed],
) -> RoutePlan {
    let centers = assign_positions(by_rank, &sizes.lay_w, GAP_X, &graph.edges, ranks);
    let (edge_bus, bus_tracks) = plan_buses(graph, ranks, &centers, max_rank, false);

    let rank_h: Vec<usize> =
        by_rank.iter().map(|row| row.iter().map(|&i| sizes.box_h[i] + sizes.extra_h[i]).max().unwrap_or(3)).collect();
    let mut rank_y = vec![0; max_rank + 1];
    for r in 1..=max_rank {
        rank_y[r] = rank_y[r - 1] + rank_h[r - 1] + GAP_Y.max(bus_tracks[r - 1] + 1);
    }
    let canvas_h = rank_y[max_rank] + rank_h[max_rank];
    let band_end = (0..=max_rank).map(|r| rank_y[r] + rank_h[r]).collect();

    let mut diagram_w = 1;
    for (r, row) in by_rank.iter().enumerate() {
        for &idx in row {
            let (w, h, cx) = (sizes.box_w[idx], sizes.box_h[idx], centers[idx]);
            let x = cx.saturating_sub(w / 2);
            let y = rank_y[r] + (rank_h[r] - h - sizes.extra_h[idx]) / 2;
            placed[idx] = Placed { x, y, w, h, cx, cy: y + h / 2, rank: r };
            diagram_w = diagram_w.max(x + w);
            if sizes.extra_h[idx] > 0 && sizes.self_label_w[idx] > 0 {
                diagram_w = diagram_w.max(x + w + 2 + sizes.self_label_w[idx]);
            }
        }
    }

    let mut content_w = diagram_w;
    for e in &graph.edges {
        let Some(label) = e.label.as_deref().filter(|_| e.from != e.to) else { continue };
        let lw = string_width(label).min(MAX_LABEL);
        content_w = if ranks[e.to] == ranks[e.from] + 1 {
            content_w.max(placed[e.to].cx + 2 + lw)
        } else {
            content_w.max(diagram_w + lw + 1)
        };
    }

    let (edge_lane, lanes) = plan_lanes(graph, ranks, placed, true);
    let (canvas_w, lane_base) = if lanes == 0 { (content_w, 0) } else { (content_w + 1 + lanes, content_w + 1) };
    RoutePlan { canvas_w, canvas_h, band_end, edge_bus, lane_base, edge_lane }
}

pub(super) fn place_lr(
    ranks: &[usize],
    max_rank: usize,
    by_rank: &[Vec<usize>],
    sizes: &NodeSizes,
    graph: &Graph,
    placed: &mut [Placed],
) -> RoutePlan {
    let col_w: Vec<usize> = by_rank.iter().map(|row| row.iter().map(|&i| sizes.box_w[i]).max().unwrap_or(0)).collect();

    // Left-to-right edge labels sit in the gap between columns, so the gap has to fit
    // the widest of them.
    let max_label = graph
        .edges
        .iter()
        .filter(|e| e.from == e.to || ranks[e.to] == ranks[e.from] + 1)
        .filter_map(|e| e.label.as_deref())
        .map(|label| string_width(label).min(MAX_LABEL))
        .max()
        .unwrap_or(0);
    let base_gap = (GAP_X + 1).max(max_label + 3);

    let centers = assign_positions(by_rank, &sizes.lay_h, 1, &graph.edges, ranks);
    let (edge_bus, bus_tracks) = plan_buses(graph, ranks, &centers, max_rank, true);

    let mut rank_x = vec![0; max_rank + 1];
    for r in 1..=max_rank {
        rank_x[r] = rank_x[r - 1] + col_w[r - 1] + base_gap.max(bus_tracks[r - 1] + 1);
    }
    let self_tail = by_rank[max_rank]
        .iter()
        .filter(|&&i| sizes.extra_h[i] > 0 && sizes.self_label_w[i] > 0)
        .map(|&i| 2 + sizes.self_label_w[i])
        .max()
        .unwrap_or(0);
    let canvas_w = rank_x[max_rank] + col_w[max_rank] + self_tail;
    let band_end = (0..=max_rank).map(|r| rank_x[r] + col_w[r]).collect();

    let mut diagram_h = 1;
    for (r, row) in by_rank.iter().enumerate() {
        let x = rank_x[r];
        for &idx in row {
            let (w, h, cy) = (sizes.box_w[idx], sizes.box_h[idx], centers[idx]);
            let y = cy.saturating_sub((h + sizes.extra_h[idx]) / 2);
            placed[idx] = Placed { x, y, w, h, cx: x + w / 2, cy: y + h / 2, rank: r };
            diagram_h = diagram_h.max(y + h + sizes.extra_h[idx]);
        }
    }

    let (edge_lane, lanes) = plan_lanes(graph, ranks, placed, false);
    let (canvas_h, lane_base) = if lanes == 0 { (diagram_h, 0) } else { (diagram_h + 1 + lanes, diagram_h + 1) };
    RoutePlan { canvas_w, canvas_h, band_end, edge_bus, lane_base, edge_lane }
}
