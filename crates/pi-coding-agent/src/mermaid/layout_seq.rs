//! Sequence diagram layout (upstream `layout-seq.ts`). Column gaps come from the
//! widest thing that has to fit between two lifelines; items stack in source order.

use super::canvas::{Canvas, D, L, R, U, draw_text_over_edges};
use super::graph::Shape;
use super::labels::{WRAP_WIDTH, fit_label};
use super::layout::MAX_CANVAS_CELLS;
use super::layout_draw::{PAD, draw_box};
use super::layout_place::Placed;
use super::parse_seq::{NoteAnchor, SeqHead, SeqItem, Sequence};
use super::types::Cls;
use super::width::string_width;

/// Minimum columns between two neighbouring lifelines.
const SEQ_GAP: usize = 5;
const BOX_H: usize = 3;

/// Where a note box sits, as `(x, w)`, given the lifeline positions.
fn note_geometry(xs: &[usize], anchor: NoteAnchor, text_w: usize) -> (usize, usize) {
    let w = text_w + 2 * PAD + 2;
    match anchor {
        NoteAnchor::Over { from, to } => {
            let center = (xs[from] + xs[to]) / 2;
            let w = (xs[to] - xs[from] + 5).max(w);
            (center.saturating_sub(w / 2), w)
        }
        NoteAnchor::Left { at } => (xs[at].saturating_sub(2 + w - 1), w),
        NoteAnchor::Right { at } => (xs[at] + 2, w),
    }
}

fn text_width(text: &Option<String>) -> usize {
    text.as_deref().map_or(0, string_width)
}

/// Gap between each pair of lifelines, widened to fit labels, notes and self-messages.
fn solve_gaps(seq: &Sequence, box_w: &[usize]) -> Vec<usize> {
    let n = box_w.len();
    let mut gaps: Vec<usize> =
        (0..n.saturating_sub(1)).map(|i| SEQ_GAP.max(box_w[i].div_ceil(2) + box_w[i + 1].div_ceil(2) + 1)).collect();

    // Each entry says "columns l..r together need at least `need` cells".
    let mut needs: Vec<(usize, usize, usize)> = Vec::new();
    for item in &seq.items {
        match item {
            SeqItem::Message { from, to, text, .. } => {
                let tw = text_width(text);
                if from != to {
                    needs.push((*from.min(to), *from.max(to), (tw + 2).max(4)));
                } else if from + 1 < n {
                    needs.push((*from, from + 1, 5 + tw + 2));
                }
            }
            SeqItem::Note { anchor, text } => {
                let tw = string_width(text);
                match *anchor {
                    NoteAnchor::Over { from, to } if from < to => needs.push((from, to, tw.saturating_sub(1))),
                    NoteAnchor::Over { from, .. } => {
                        let need = (tw + 4).div_ceil(2) + 2;
                        if from > 0 {
                            needs.push((from - 1, from, need));
                        }
                        if from + 1 < n {
                            needs.push((from, from + 1, need));
                        }
                    }
                    NoteAnchor::Left { at } if at > 0 => needs.push((at - 1, at, tw + 7)),
                    NoteAnchor::Right { at } if at + 1 < n => needs.push((at, at + 1, tw + 7)),
                    _ => {}
                }
            }
            SeqItem::Divider { .. } => {}
        }
    }
    // Narrowest spans first, so a wide span absorbs what they already gave.
    needs.sort_by_key(|&(l, r, _)| r - l);
    for (l, r, need) in needs {
        let cur: usize = gaps[l..r].iter().sum();
        if cur < need {
            gaps[r - 1] += need - cur;
        }
    }
    gaps
}

fn row_height(item: &SeqItem) -> usize {
    match item {
        SeqItem::Note { .. } => 4,
        SeqItem::Divider { .. } => 2,
        SeqItem::Message { from, to, .. } if from == to => 4,
        SeqItem::Message { text: Some(_), .. } => 3,
        SeqItem::Message { text: None, .. } => 2,
    }
}

/// Geometry for a box drawn by position and size; ranks are irrelevant here.
fn boxed(x: usize, y: usize, w: usize, h: usize) -> Placed {
    Placed { x, y, w, h, cx: x + w / 2, cy: y + 1, rank: 0 }
}

pub(super) fn layout_sequence(seq: &Sequence) -> Option<Canvas> {
    let n = seq.labels.len();
    let labels: Vec<String> = seq.labels.iter().map(|l| fit_label(l, WRAP_WIDTH)).collect();
    let box_w: Vec<usize> = labels.iter().map(|l| string_width(l).max(1) + 2 * PAD + 2).collect();
    let gaps = solve_gaps(seq, &box_w);

    let mut xs = vec![box_w[0] / 2; n];
    for i in 1..n {
        xs[i] = xs[i - 1] + gaps[i - 1];
    }

    let mut canvas_w = xs[n - 1] + box_w[n - 1].div_ceil(2) + 1;
    for item in &seq.items {
        match item {
            SeqItem::Message { from, to, text, .. } if from == to => {
                canvas_w = canvas_w.max(xs[*from] + 5 + text_width(text) + 1);
            }
            SeqItem::Note { anchor, text } => {
                let (x, w) = note_geometry(&xs, *anchor, string_width(text));
                canvas_w = canvas_w.max(x + w + 1);
            }
            SeqItem::Divider { text } => canvas_w = canvas_w.max(string_width(text) + 4),
            SeqItem::Message { .. } => {}
        }
    }

    let mut rows = Vec::with_capacity(seq.items.len());
    let mut y = BOX_H + 1;
    for item in &seq.items {
        rows.push(y);
        y += row_height(item);
    }
    let bottom_top = y;
    let canvas_h = bottom_top + BOX_H;
    if canvas_w * canvas_h > MAX_CANVAS_CELLS {
        return None;
    }

    let mut canvas = Canvas::new(canvas_w, canvas_h);
    for i in 0..n {
        for by in [0, bottom_top] {
            let placed = boxed(xs[i].saturating_sub(box_w[i] / 2), by, box_w[i], BOX_H);
            draw_box(&mut canvas, &placed, std::slice::from_ref(&labels[i]), Shape::Rect);
        }
    }
    for (item, &row) in seq.items.iter().zip(&rows) {
        if let SeqItem::Note { anchor, text } = item {
            let (x, w) = note_geometry(&xs, *anchor, string_width(text));
            draw_box(&mut canvas, &boxed(x, row, w, 3), std::slice::from_ref(text), Shape::Rect);
        }
    }
    for &x in &xs {
        canvas.junction(x, BOX_H - 1, D);
        canvas.seg_v(x, BOX_H, bottom_top - 1);
        canvas.junction(x, bottom_top, U);
    }
    for (item, &row) in seq.items.iter().zip(&rows) {
        match item {
            SeqItem::Message { from, to, text, dashed, head } => {
                draw_message(&mut canvas, (*from, *to), text.as_deref(), *dashed, *head, &xs, row);
            }
            SeqItem::Divider { text } => draw_divider(&mut canvas, text, row, canvas_w),
            SeqItem::Note { .. } => {}
        }
    }

    canvas.finalize_mask();
    Some(canvas)
}

fn draw_message(
    canvas: &mut Canvas,
    (from, to): (usize, usize),
    text: Option<&str>,
    dashed: bool,
    head: SeqHead,
    xs: &[usize],
    r: usize,
) {
    let line_ch = if dashed { '╌' } else { '─' };

    if from == to {
        // A stub that leaves the lifeline and returns two rows down.
        let x = xs[from];
        canvas.junction(x, r, R);
        canvas.set_char(x + 1, r, line_ch, Cls::Edge);
        canvas.set_char(x + 2, r, line_ch, Cls::Edge);
        canvas.set_char(x + 3, r, '╮', Cls::Edge);
        canvas.set_char(x + 3, r + 1, '│', Cls::Edge);
        canvas.set_char(x + 1, r + 2, if head == SeqHead::Cross { '×' } else { '◄' }, Cls::Edge);
        canvas.set_char(x + 2, r + 2, line_ch, Cls::Edge);
        canvas.set_char(x + 3, r + 2, '╯', Cls::Edge);
        if let Some(text) = text {
            draw_text_over_edges(canvas, text, x + 5, r + 1, Cls::Text);
        }
        return;
    }

    let (x0, x1) = (xs[from], xs[to]);
    let rightward = x1 > x0;
    // A labelled message writes its text on `r` and draws the arrow below it.
    let arrow_row = if text.is_some() { r + 1 } else { r };
    let (lo, hi) = (x0.min(x1), x0.max(x1));

    canvas.junction(x0, arrow_row, if rightward { R } else { L });
    for x in lo + 1..hi {
        canvas.set_char(x, arrow_row, line_ch, Cls::Edge);
    }
    let head_ch = match head {
        SeqHead::Cross => '×',
        SeqHead::Arrow if rightward => '▶',
        SeqHead::Arrow => '◄',
    };
    canvas.set_char(if rightward { x1 - 1 } else { x1 + 1 }, arrow_row, head_ch, Cls::Edge);

    if let Some(text) = text {
        let span = hi - lo - 1;
        let t = fit_label(text, span.max(1));
        draw_text_over_edges(canvas, &t, lo + 1 + span.saturating_sub(string_width(&t)) / 2, r, Cls::Text);
    }
}

/// A full-width rule labelling a `loop`, `alt` or `opt` block boundary.
fn draw_divider(canvas: &mut Canvas, text: &str, r: usize, canvas_w: usize) {
    for x in 0..canvas_w {
        canvas.set_char(x, r, '─', Cls::Edge);
    }
    let label = format!(" {} ", fit_label(text, canvas_w.saturating_sub(4)));
    draw_text_over_edges(canvas, &label, 2, r, Cls::EdgeLabel);
}
