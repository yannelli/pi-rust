//! Mermaid diagrams as Unicode box-drawing art for terminals, a port of the npm package
//! `grok-mermaid` 0.2.3. Draws flowcharts (with subgraphs), state, class, ER and
//! sequence diagrams; [`source_box`] frames the raw source when art is unwanted.

mod ansi;
mod canvas;
mod graph;
mod labels;
mod layout;
mod layout_draw;
mod layout_place;
mod layout_rank;
mod layout_seq;
mod parse_flow;
mod parse_seq;
mod parse_state;
mod source_box;
mod strings;
mod types;
mod width;
mod width_data;

#[cfg(test)]
mod tests;

pub use ansi::{AnsiTheme, DEFAULT_THEME, to_ansi};
pub use parse_flow::{DiagramKind, diagram_kind};
pub use source_box::source_box;
pub use types::{Cls, MermaidArt, Span};

use canvas::Canvas;

/// Render a Mermaid source block as art at whatever size it needs. `None` means blank
/// input, a syntax error, an unsupported type, or a diagram too large to lay out.
/// Parsing is best-effort; anything dropped is listed in the advisory `warnings`.
pub fn render(src: &str) -> Option<MermaidArt> {
    let src = strings::strip_controls(src);
    if strings::trim(&src).is_empty() {
        return None;
    }
    let (canvas, warnings) = attempt(&src)?;
    let (plain, styled, width) = canvas.to_lines();
    Some(MermaidArt { plain, styled, width, warnings })
}

/// Draw `src`, retrying once without its last line if the grammar rejects it. While a
/// source streams in, its last line is usually still being typed; dropping it is always
/// reported, so a finished document with a bad last line still says what it lost.
fn attempt(src: &str) -> Option<(Canvas, Vec<String>)> {
    if let Some(drawn) = draw(src) {
        return Some(drawn);
    }
    let body = strings::trim_end(src);
    let cut = body.rfind('\n')?;
    let (canvas, mut warnings) = draw(&body[..cut])?;
    let dropped = strings::trim(&body[cut + 1..]);
    warnings.push(format!("dropped, unreadable final line: \"{dropped}\""));
    Some((canvas, warnings))
}

/// Dispatch on the declared diagram type; `None` means nothing was drawn.
fn draw(src: &str) -> Option<(Canvas, Vec<String>)> {
    let plain = |canvas: Option<Canvas>| canvas.map(|canvas| (canvas, Vec::new()));
    match diagram_kind(src)? {
        DiagramKind::Flowchart => {
            let graph = parse_flow::parse_graph(src)?;
            let canvas =
                if graph.groups.is_empty() { layout::layout_flowchart(&graph) } else { layout::layout_grouped(&graph) };
            canvas.map(|canvas| (canvas, graph.warnings))
        }
        DiagramKind::State => plain(layout::layout_flowchart(&parse_state::parse_state(src)?)),
        DiagramKind::Class => {
            let (graph, infos) = parse_state::parse_class(src)?;
            plain(layout::layout_class(&graph, &infos))
        }
        DiagramKind::Er => {
            let (graph, infos) = parse_seq::parse_er(src)?;
            plain(layout::layout_class(&graph, &infos))
        }
        DiagramKind::Sequence => plain(layout_seq::layout_sequence(&parse_seq::parse_sequence(src)?)),
    }
}
