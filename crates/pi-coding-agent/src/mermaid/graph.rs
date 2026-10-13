//! The shared diagram model (upstream `graph.ts`). Flowchart, state, class and ER
//! sources all parse into a [`Graph`]; only sequence diagrams have their own model.

use std::collections::HashMap;

/// Caps that keep layout bounded; exceeding one drops the diagram.
pub(super) const MAX_NODES: usize = 128;
pub(super) const MAX_EDGES: usize = 512;
pub(super) const MAX_GROUPS: usize = 24;
pub(super) const MAX_GROUP_DEPTH: usize = 6;
/// Class members and ER attributes listed per box before the rest become an ellipsis.
pub(super) const MAX_MEMBERS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Shape {
    Rect,
    Round,
    Diamond,
}

/// Decoration at one end of an edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Head {
    None,
    Arrow,
    Circle,
    Cross,
    Triangle,
    DiamondFill,
    DiamondOpen,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LineKind {
    Solid,
    Dotted,
    Thick,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Dir {
    Down,
    Up,
    Right,
    Left,
}

#[derive(Clone, Debug)]
pub(super) struct Node {
    pub label: String,
    pub shape: Shape,
}

#[derive(Clone, Debug)]
pub(super) struct Edge {
    pub from: usize,
    pub to: usize,
    pub label: Option<String>,
    pub head_to: Head,
    pub head_from: Head,
    pub line: LineKind,
}

#[derive(Clone, Debug)]
pub(super) struct Group {
    pub id: String,
    pub label: String,
    pub parent: Option<usize>,
}

/// Extra compartment content for class and ER boxes.
#[derive(Clone, Debug, Default)]
pub(super) struct ClassInfo {
    pub annotation: Option<String>,
    pub attrs: Vec<String>,
    pub methods: Vec<String>,
}

/// `LR`, `RL` or `BT` as written in a header or `direction` statement; else down.
pub(super) fn parse_dir(token: &str) -> Dir {
    match token.to_ascii_uppercase().as_str() {
        "LR" => Dir::Right,
        "RL" => Dir::Left,
        "BT" => Dir::Up,
        _ => Dir::Down,
    }
}

#[derive(Debug)]
pub(super) struct Graph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub index: HashMap<String, usize>,
    pub groups: Vec<Group>,
    /// Innermost subgraph each node was declared in, parallel to `nodes`.
    pub node_group: Vec<Option<usize>>,
    pub cur_group: Option<usize>,
    /// Set when a cap was hit; the caller abandons the parse.
    pub over_cap: bool,
    /// Flowchart text that could not be read and was dropped.
    pub warnings: Vec<String>,
    pub dir: Dir,
}

impl Graph {
    pub(super) fn new(dir: Dir) -> Self {
        Graph {
            nodes: Vec::new(),
            edges: Vec::new(),
            index: HashMap::new(),
            groups: Vec::new(),
            node_group: Vec::new(),
            cur_group: None,
            over_cap: false,
            warnings: Vec::new(),
            dir,
        }
    }

    /// Index of `id`, creating the node if new. A later declaration with a label
    /// overwrites the placeholder an edge created. `None` once `MAX_NODES` is reached.
    pub(super) fn node_index(&mut self, id: &str, label: Option<String>, shape: Shape) -> Option<usize> {
        if let Some(&existing) = self.index.get(id) {
            if let Some(label) = label {
                self.nodes[existing].label = label;
                self.nodes[existing].shape = shape;
            }
            return Some(existing);
        }
        if self.nodes.len() >= MAX_NODES {
            self.over_cap = true;
            return None;
        }
        self.index.insert(id.to_string(), self.nodes.len());
        self.nodes.push(Node { label: label.unwrap_or_else(|| id.to_string()), shape });
        self.node_group.push(self.cur_group);
        Some(self.nodes.len() - 1)
    }

    /// Set a node's label without disturbing its shape, creating it if new.
    pub(super) fn node_label(&mut self, id: &str, label: String) -> Option<usize> {
        if let Some(&existing) = self.index.get(id) {
            self.nodes[existing].label = label;
            return Some(existing);
        }
        self.node_index(id, Some(label), Shape::Round)
    }

    /// Append an edge, or flag `over_cap` when `MAX_EDGES` is reached.
    pub(super) fn push_edge(&mut self, edge: Edge) -> bool {
        if self.edges.len() >= MAX_EDGES {
            self.over_cap = true;
            return false;
        }
        self.edges.push(edge);
        true
    }
}
