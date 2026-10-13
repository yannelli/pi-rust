//! The raw source in a framed box (upstream `source-box.ts`): what to show when
//! `render` returns `None`, or returns art too wide for the space at hand.

use super::strings::{first_word, src_lines, strip_controls, trim_end};
use super::types::{Cls, MermaidArt, Span};
use super::width::{measured, string_width};

/// Frame `src` in a titled box, hard-wrapping its lines to `max_width` columns. The body
/// wraps to `max(8, max_width - 4)` and the ` mermaid: <kind> ` title is never cut, so
/// the result can still be wider than `max_width`; check `width` if it matters.
pub fn source_box(src: &str, max_width: Option<usize>) -> MermaidArt {
    let src = strip_controls(src);
    let header = match first_word(&src) {
        "" => "diagram",
        word => word,
    };
    let title = format!(" mermaid: {header} ");
    let limit = max_width.map(|w| w.saturating_sub(4).max(8));

    let mut body: Vec<String> = Vec::new();
    for line in src_lines(&src).into_iter().map(trim_end) {
        if body.is_empty() && line.is_empty() {
            continue;
        }
        body.extend(chunk_line(line, limit));
    }

    let title_w = string_width(&title);
    let content_w = body.iter().map(|l| string_width(l)).fold(title_w, usize::max);
    let inner = content_w + 2;

    let mut plain = Vec::with_capacity(body.len() + 2);
    let mut styled = Vec::with_capacity(body.len() + 2);
    let span = |text: String, cls: Cls| Span { text, cls };

    let rule = "─".repeat(inner.saturating_sub(title_w));
    plain.push(format!("╭{title}{rule}╮"));
    styled.push(vec![
        span("╭".into(), Cls::Border),
        span(title.clone(), Cls::Title),
        span(format!("{rule}╮"), Cls::Border),
    ]);

    for line in body {
        let pad = " ".repeat(content_w.saturating_sub(string_width(&line)));
        plain.push(format!("│ {line}{pad} │"));
        styled.push(vec![span("│ ".into(), Cls::Border), span(line, Cls::Text), span(format!("{pad} │"), Cls::Border)]);
    }

    let bottom = format!("╰{}╯", "─".repeat(inner));
    plain.push(bottom.clone());
    styled.push(vec![span(bottom, Cls::Border)]);

    MermaidArt { plain, styled, width: inner + 2, warnings: Vec::new() }
}

/// Hard-break a line at `limit` columns, never splitting a wide glyph.
fn chunk_line(line: &str, limit: Option<usize>) -> Vec<String> {
    let Some(limit) = limit.filter(|&limit| string_width(line) > limit) else {
        return vec![line.to_string()];
    };
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0;
    for (c, cw) in measured(line) {
        if cur_w + cw > limit && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        cur.push_str(c);
        cur_w += cw;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}
