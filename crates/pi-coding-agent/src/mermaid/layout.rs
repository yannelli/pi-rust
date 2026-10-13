//! Graph layout entry points: rank, order, place, route, draw (upstream `layout.ts`).
//! `BT` and `RL` reuse the `TD` and `LR` layouts and flip the finished canvas.

use std::collections::HashMap;

use super::canvas::{Canvas, STY_DOT, STY_SOLID, STY_THICK};
use super::graph::{ClassInfo, Dir, Edge, Graph, LineKind, Node, Shape};
use super::labels::{MAX_LABEL, MAX_LINES, WRAP_WIDTH, fit_label, wrap_label};
use super::layout_draw::{
    PAD, draw_box, draw_class_box, draw_frame, route_back, route_back_lr, route_forward, route_forward_lr, route_self,
};
use super::layout_place::{NodeSizes, Placed, place_lr, place_td};
use super::layout_rank::{compute_ranks, order_ranks};
use super::parse_state::display_generics;
use super::width::string_width;

/// Refuse to allocate a canvas larger than this many cells.
pub(super) const MAX_CANVAS_CELLS: usize = 1 << 21;

/// What to draw inside a node box.
pub(super) enum NodeExtra {
    Plain,
    Frame(Canvas),
    Compartments(Vec<Vec<String>>),
}

fn widest<'a>(lines: impl Iterator<Item = &'a String>) -> usize {
    lines.map(|l| string_width(l)).max().unwrap_or(1).max(1)
}

/// Rank, place, draw and route a graph onto a fresh canvas. `None` when the graph is
/// empty or the canvas would exceed the cell cap.
fn layout_canvas(graph: &Graph, extras: &[NodeExtra]) -> Option<Canvas> {
    let n = graph.nodes.len();
    if n == 0 {
        return None;
    }
    let ranks = compute_ranks(graph);
    let max_rank = ranks.iter().copied().max().unwrap_or(0);
    let mut by_rank = vec![Vec::new(); max_rank + 1];
    for (idx, &r) in ranks.iter().enumerate() {
        by_rank[r].push(idx);
    }
    order_ranks(&mut by_rank, &graph.edges, &ranks);

    let wrapped: Vec<Vec<String>> =
        graph.nodes.iter().map(|node| wrap_label(&node.label, WRAP_WIDTH, MAX_LINES)).collect();
    let mut box_w: Vec<usize> = Vec::with_capacity(n);
    let mut box_h: Vec<usize> = Vec::with_capacity(n);
    for (i, extra) in extras.iter().enumerate() {
        let (w, h) = match extra {
            NodeExtra::Frame(sub) => {
                ((sub.w + 2).max(string_width(&fit_label(&graph.nodes[i].label, WRAP_WIDTH)) + 4), sub.h + 2)
            }
            NodeExtra::Compartments(sections) => {
                let filled = sections.iter().filter(|s| !s.is_empty()).count();
                let rows: usize = sections.iter().map(Vec::len).sum();
                (widest(sections.iter().flatten()) + 2 * PAD + 2, rows + filled.saturating_sub(1) + 2)
            }
            NodeExtra::Plain => (widest(wrapped[i].iter()) + 2 * PAD + 2, wrapped[i].len() + 2),
        };
        box_w.push(w);
        box_h.push(h);
    }

    // A self-edge needs two rows below its box, and room beside it for a label.
    let mut extra_h = vec![0; n];
    let mut self_label_w = vec![0; n];
    for e in graph.edges.iter().filter(|e| e.from == e.to) {
        extra_h[e.from] = 2;
        if let Some(label) = &e.label {
            self_label_w[e.from] = self_label_w[e.from].max(string_width(label).min(MAX_LABEL));
        }
    }
    for i in 0..n {
        if extra_h[i] > 0 {
            box_w[i] = box_w[i].max(7);
        }
    }

    let lay_w = (0..n).map(|i| box_w[i] + if self_label_w[i] > 0 { 2 * (self_label_w[i] + 3) } else { 0 }).collect();
    let lay_h = (0..n).map(|i| box_h[i] + extra_h[i]).collect();
    let sizes = NodeSizes { box_w, box_h, lay_w, lay_h, extra_h, self_label_w };

    let mut placed = vec![Placed::default(); n];
    let vertical = matches!(graph.dir, Dir::Down | Dir::Up);
    let plan = if vertical {
        place_td(&ranks, max_rank, &by_rank, &sizes, graph, &mut placed)
    } else {
        place_lr(&ranks, max_rank, &by_rank, &sizes, graph, &mut placed)
    };
    if plan.canvas_w * plan.canvas_h > MAX_CANVAS_CELLS {
        return None;
    }

    let mut canvas = Canvas::new(plan.canvas_w, plan.canvas_h);
    for (idx, extra) in extras.iter().enumerate() {
        match extra {
            NodeExtra::Frame(sub) => draw_frame(&mut canvas, &placed[idx], &graph.nodes[idx].label, sub),
            NodeExtra::Compartments(sections) => draw_class_box(&mut canvas, &placed[idx], sections),
            NodeExtra::Plain => draw_box(&mut canvas, &placed[idx], &wrapped[idx], graph.nodes[idx].shape),
        }
    }

    for (i, edge) in graph.edges.iter().enumerate() {
        canvas.cur_style = match edge.line {
            LineKind::Dotted => STY_DOT,
            LineKind::Thick => STY_THICK,
            LineKind::Solid => STY_SOLID,
        };
        if edge.from == edge.to {
            route_self(&mut canvas, &placed[edge.from], edge);
            continue;
        }
        let (from, to) = (&placed[edge.from], &placed[edge.to]);
        let adjacent = to.rank == from.rank + 1;
        let bus = plan.band_end[from.rank] + plan.edge_bus[i];
        let lane = plan.lane_base + plan.edge_lane[i];
        match (vertical, adjacent) {
            (true, true) => route_forward(&mut canvas, from, to, edge, bus),
            (true, false) => route_back(&mut canvas, from, to, edge, lane),
            (false, true) => route_forward_lr(&mut canvas, from, to, edge, bus),
            (false, false) => route_back_lr(&mut canvas, from, to, edge, lane),
        }
    }

    canvas.finalize_mask();
    Some(canvas)
}

/// Apply the flip a finished canvas needs for `BT` or `RL`.
fn orient(mut canvas: Canvas, dir: Dir) -> Canvas {
    match dir {
        Dir::Up => canvas.flip_vertical(),
        Dir::Left => canvas.flip_horizontal(),
        Dir::Down | Dir::Right => {}
    }
    canvas
}

/// Flowchart and state diagrams: plain boxes, no extra content.
pub(super) fn layout_flowchart(graph: &Graph) -> Option<Canvas> {
    let extras: Vec<NodeExtra> = graph.nodes.iter().map(|_| NodeExtra::Plain).collect();
    layout_canvas(graph, &extras).map(|canvas| orient(canvas, graph.dir))
}

/// Class and ER diagrams: boxes divided into title, attribute and method rows.
pub(super) fn layout_class(graph: &Graph, infos: &[ClassInfo]) -> Option<Canvas> {
    let extras: Vec<NodeExtra> = graph
        .nodes
        .iter()
        .zip(infos)
        .map(|(node, info)| {
            let mut title = Vec::new();
            if let Some(annotation) = &info.annotation {
                title.push(format!("\u{ab}{annotation}\u{bb}"));
            }
            title.push(display_generics(&node.label));
            NodeExtra::Compartments(vec![title, info.attrs.clone(), info.methods.clone()])
        })
        .collect();
    layout_canvas(graph, &extras).map(|canvas| orient(canvas, graph.dir))
}

/// An item laid out in one subgraph scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Item {
    Node(usize),
    Group(usize),
}

type ScopeEdges = HashMap<Option<usize>, Vec<(Item, Item, usize)>>;

/// Lay out a flowchart that uses `subgraph`. Each subgraph is a framed box holding its
/// own canvas; an edge is drawn in the innermost scope holding both endpoints, and
/// attaches to the frame when it crosses a subgraph boundary.
pub(super) fn layout_grouped(graph: &Graph) -> Option<Canvas> {
    // A node whose id matches a subgraph id stands in for that subgraph.
    let mut proxy: HashMap<usize, usize> = HashMap::new();
    for (gi, g) in graph.groups.iter().enumerate() {
        if let Some(&ni) = graph.index.get(&g.id) {
            proxy.insert(ni, gi);
        }
    }
    let group_chain = |mut cur: Option<usize>| {
        let mut chain = Vec::new();
        while let Some(g) = cur {
            chain.push(g);
            cur = graph.groups[g].parent;
        }
        chain.reverse();
        chain
    };
    let endpoint = |n: usize| match proxy.get(&n) {
        None => (Item::Node(n), group_chain(graph.node_group[n])),
        Some(&gi) => (Item::Group(gi), group_chain(graph.groups[gi].parent)),
    };

    let mut scope_edges: ScopeEdges = HashMap::new();
    let mut referenced = vec![false; graph.groups.len()];
    for (ei, e) in graph.edges.iter().enumerate() {
        let (f_key, f_chain) = endpoint(e.from);
        let (t_key, t_chain) = endpoint(e.to);
        let k = f_chain.iter().zip(&t_chain).take_while(|(a, b)| a == b).count();
        let scope = k.checked_sub(1).map(|i| f_chain[i]);
        let f_item = f_chain.get(k).map_or(f_key, |&g| Item::Group(g));
        let t_item = t_chain.get(k).map_or(t_key, |&g| Item::Group(g));
        for item in [f_item, t_item] {
            if let Item::Group(g) = item {
                referenced[g] = true;
            }
        }
        scope_edges.entry(scope).or_default().push((f_item, t_item, ei));
    }

    let mut direct_nodes: HashMap<Option<usize>, Vec<usize>> = HashMap::new();
    for (ni, &g) in graph.node_group.iter().enumerate() {
        if !proxy.contains_key(&ni) {
            direct_nodes.entry(g).or_default().push(ni);
        }
    }

    // Drop empty subgraphs, but keep any that an edge attaches to.
    let mut keep = vec![false; graph.groups.len()];
    for gi in (0..graph.groups.len()).rev() {
        let has_nodes = direct_nodes.get(&Some(gi)).is_some_and(|v| !v.is_empty());
        let has_children = graph.groups.iter().enumerate().any(|(c, g)| g.parent == Some(gi) && keep[c]);
        keep[gi] = has_nodes || has_children || referenced[gi];
    }

    let scopes = Scopes { graph, scope_edges, direct_nodes, keep };
    scopes.build(None).map(|canvas| orient(canvas, graph.dir))
}

struct Scopes<'a> {
    graph: &'a Graph,
    scope_edges: ScopeEdges,
    direct_nodes: HashMap<Option<usize>, Vec<usize>>,
    keep: Vec<bool>,
}

impl Scopes<'_> {
    fn build(&self, scope: Option<usize>) -> Option<Canvas> {
        let graph = self.graph;
        let mut items: Vec<Item> =
            self.direct_nodes.get(&scope).map(|v| v.iter().map(|&n| Item::Node(n)).collect()).unwrap_or_default();
        items.extend(
            (0..graph.groups.len()).filter(|&gi| graph.groups[gi].parent == scope && self.keep[gi]).map(Item::Group),
        );
        if items.is_empty() {
            return Some(Canvas::new(1, 1));
        }

        let mut index_of: HashMap<Item, usize> = HashMap::new();
        let mut nodes = Vec::with_capacity(items.len());
        let mut extras = Vec::with_capacity(items.len());
        for &item in &items {
            index_of.insert(item, nodes.len());
            match item {
                Item::Node(i) => {
                    nodes.push(graph.nodes[i].clone());
                    extras.push(NodeExtra::Plain);
                }
                Item::Group(i) => {
                    let sub = self.build(Some(i))?;
                    nodes.push(Node { label: graph.groups[i].label.clone(), shape: Shape::Rect });
                    extras.push(NodeExtra::Frame(sub));
                }
            }
        }

        let mut synth = Graph::new(graph.dir);
        for &(f, t, ei) in self.scope_edges.get(&scope).map_or(&[][..], Vec::as_slice) {
            let (Some(&from), Some(&to)) = (index_of.get(&f), index_of.get(&t)) else { continue };
            synth.edges.push(Edge { from, to, ..graph.edges[ei].clone() });
        }
        // Layout only reads nodes, edges and dir, so a bare Graph carrying those is enough.
        synth.nodes = nodes;
        layout_canvas(&synth, &extras)
    }
}
