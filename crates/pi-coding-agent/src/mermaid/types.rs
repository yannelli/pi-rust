//! Output model shared by every diagram kind (upstream `types.ts`).

/// Semantic class of a run of cells. The renderer knows nothing about colour;
/// callers map classes to their own theme (see [`super::to_ansi`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cls {
    /// Box outlines, subgraph frames, compartment rules.
    Border,
    /// Node, participant and compartment labels.
    Text,
    /// Connector lines and arrowheads.
    Edge,
    /// Text sitting on an edge.
    EdgeLabel,
    /// The `mermaid: <kind>` header of a source box.
    Title,
    /// Blank filler.
    None,
}

impl Cls {
    /// The upstream class name (`"border"`, `"edgeLabel"`, ...).
    pub fn as_str(self) -> &'static str {
        match self {
            Cls::Border => "border",
            Cls::Text => "text",
            Cls::Edge => "edge",
            Cls::EdgeLabel => "edgeLabel",
            Cls::Title => "title",
            Cls::None => "none",
        }
    }
}

/// A run of cells in one row that share one semantic class.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub cls: Cls,
}

/// A rendered diagram. `plain[i]` and `styled[i]` describe the same row: `plain` is
/// right-trimmed, `styled` keeps the runs needed to colour it. `width` is the display
/// columns the widest row needs. `warnings` lists source that was dropped (advisory only).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MermaidArt {
    pub plain: Vec<String>,
    pub styled: Vec<Vec<Span>>,
    pub width: usize,
    pub warnings: Vec<String>,
}
