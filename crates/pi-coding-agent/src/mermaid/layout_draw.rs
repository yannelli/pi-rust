//! Box drawing and edge routing (upstream `layout.ts`).

use super::canvas::{Canvas, D, L, R, U, draw_text, draw_text_over_edges};
use super::graph::{Edge, Head, LineKind, Shape};
use super::labels::{MAX_LABEL, fit_label};
use super::layout_place::Placed;
use super::types::Cls;
use super::width::{measured, string_width};

/// Cells of padding between a box border and its text.
pub(super) const PAD: usize = 1;

pub(super) fn draw_box(canvas: &mut Canvas, p: &Placed, lines: &[String], shape: Shape) {
    let Placed { x, y, w, h, .. } = *p;
    let right = x + w - 1;
    let bottom = y + h - 1;

    let rounded = matches!(shape, Shape::Round | Shape::Diamond);
    canvas.set_char(x, y, if rounded { '╭' } else { '┌' }, Cls::Border);
    canvas.set_char(right, y, if rounded { '╮' } else { '┐' }, Cls::Border);
    canvas.set_char(x, bottom, if rounded { '╰' } else { '└' }, Cls::Border);
    canvas.set_char(right, bottom, if rounded { '╯' } else { '┘' }, Cls::Border);

    // The perimeter is drawn as bits so edges can tee into it, but it is the box
    // outline, so it claims `border` rather than `edge`.
    for cx in x + 1..right {
        canvas.add_bits(cx, y, L | R, Cls::Border);
        canvas.add_bits(cx, bottom, L | R, Cls::Border);
    }
    for cy in y + 1..bottom {
        canvas.add_bits(x, cy, U | D, Cls::Border);
        canvas.add_bits(right, cy, U | D, Cls::Border);
    }
    for cy in y..=bottom.min(canvas.h.saturating_sub(1)) {
        for cx in x..=right.min(canvas.w.saturating_sub(1)) {
            let i = canvas.idx(cx, cy);
            canvas.occupied[i] = true;
        }
    }

    let inner = w.saturating_sub(2 * PAD + 2).max(1);
    for (li, line) in lines.iter().enumerate() {
        let text = fit_label(line, inner);
        let text_x = x + 1 + PAD + inner.saturating_sub(string_width(&text)) / 2;
        draw_text(canvas, &text, text_x, y + 1 + li, Cls::Text);
    }
}

/// A class or ER box: sections separated by horizontal rules, title centred.
pub(super) fn draw_class_box(canvas: &mut Canvas, p: &Placed, sections: &[Vec<String>]) {
    draw_box(canvas, p, &[], Shape::Rect);
    let inner = p.w.saturating_sub(2 * PAD + 2).max(1);
    let mut row = p.y + 1;
    let mut first = true;
    for (si, section) in sections.iter().enumerate() {
        if section.is_empty() {
            continue;
        }
        if !first {
            canvas.set_char(p.x, row, '├', Cls::Border);
            for x in p.x + 1..p.x + p.w - 1 {
                canvas.set_char(x, row, '─', Cls::Border);
            }
            canvas.set_char(p.x + p.w - 1, row, '┤', Cls::Border);
            row += 1;
        }
        first = false;
        for line in section {
            let text = fit_label(line, inner);
            let centre = inner.saturating_sub(string_width(&text)) / 2;
            let tx = p.x + 1 + PAD + if si == 0 { centre } else { 0 };
            draw_text_over_edges(canvas, &text, tx, row, Cls::Text);
            row += 1;
        }
    }
}

/// A subgraph frame: a titled box with a finished sub-canvas centred inside.
pub(super) fn draw_frame(canvas: &mut Canvas, p: &Placed, title: &str, sub: &Canvas) {
    draw_box(canvas, p, &[], Shape::Rect);
    let t = fit_label(title, p.w.saturating_sub(4));
    draw_text_over_edges(canvas, &format!(" {t} "), p.x + 1, p.y, Cls::Text);
    canvas.blit(sub, p.x + 1 + (p.w - 2 - sub.w) / 2, p.y + 1 + (p.h - 2 - sub.h) / 2);
}

fn head_glyph(head: Head, arrow: char) -> char {
    match head {
        Head::Circle => 'o',
        Head::Cross => '×',
        Head::DiamondFill => '◆',
        Head::DiamondOpen => '◇',
        Head::Triangle => match arrow {
            '▼' => '▽',
            '▲' => '△',
            '◄' => '◁',
            '▶' => '▷',
            _ => arrow,
        },
        Head::None | Head::Arrow => arrow,
    }
}

/// Draw the head of an edge, or let a headless edge end in plain bits.
fn end_head(canvas: &mut Canvas, x: usize, y: usize, head: Head, arrow: char, bits: u8) {
    if head == Head::None {
        canvas.add_bits(x, y, bits, Cls::Edge);
    } else {
        canvas.set_char(x, y, head_glyph(head, arrow), Cls::Edge);
    }
}

/// Draw the tail decoration of an edge, if it has one.
fn start_head(canvas: &mut Canvas, x: usize, y: usize, head: Head, arrow: char) {
    if head != Head::None {
        canvas.set_char(x, y, head_glyph(head, arrow), Cls::Edge);
    }
}

/// Rank to next rank, top-down: drop, jog along the bus row, drop into the head.
pub(super) fn route_forward(canvas: &mut Canvas, from: &Placed, to: &Placed, edge: &Edge, bus: usize) {
    let tx = to.cx;
    // A jog of one column reads as a kink; snap straight instead.
    let bx = if from.cx.abs_diff(tx) <= 1 { tx } else { from.cx };
    let by = from.y + from.h - 1;
    let head_row = to.y - 1;

    canvas.junction(bx, by, D);
    canvas.seg_v(bx, by, bus);
    if bx == tx {
        canvas.seg_v(bx, bus, head_row);
    } else {
        canvas.seg_h(bus, bx, tx);
        canvas.seg_v(tx, bus, head_row);
    }
    end_head(canvas, tx, head_row, edge.head_to, '▼', U);
    start_head(canvas, bx, by, edge.head_from, '▲');
    if let Some(label) = &edge.label {
        place_label(canvas, label, head_row, tx + 1);
    }
}

/// A self-edge: a stub loop hanging below the box.
pub(super) fn route_self(canvas: &mut Canvas, p: &Placed, edge: &Edge) {
    let bottom = p.y + p.h - 1;
    let exit_x = p.cx + 1;
    let ret_x = p.x + p.w - 2;
    if ret_x <= exit_x || bottom + 2 >= canvas.h {
        return;
    }
    let (v, h, bl, br) = match edge.line {
        LineKind::Dotted => ('╎', '╌', '╰', '╯'),
        LineKind::Thick => ('┃', '━', '┗', '┛'),
        LineKind::Solid => ('│', '─', '╰', '╯'),
    };
    canvas.junction(exit_x, bottom, D);
    canvas.set_char(exit_x, bottom + 1, v, Cls::Edge);
    canvas.set_char(exit_x, bottom + 2, bl, Cls::Edge);
    for x in exit_x + 1..ret_x {
        canvas.set_char(x, bottom + 2, h, Cls::Edge);
    }
    canvas.set_char(ret_x, bottom + 2, br, Cls::Edge);
    canvas.set_char(ret_x, bottom + 1, head_glyph(edge.head_to, '▲'), Cls::Edge);
    if let Some(label) = &edge.label {
        place_label(canvas, label, bottom + 1, p.x + p.w + 1);
    }
}

/// Skip or back edge, top-down: out the right side, along a lane, back in.
pub(super) fn route_back(canvas: &mut Canvas, from: &Placed, to: &Placed, edge: &Edge, lane_x: usize) {
    let (sx, sy) = (from.x + from.w - 1, from.cy);
    let (tx, tyc) = (to.x + to.w - 1, to.cy);

    canvas.junction(sx, sy, R);
    canvas.seg_h(sy, sx, lane_x);
    canvas.seg_v(lane_x, sy, tyc);
    canvas.seg_h(tyc, tx + 1, lane_x);
    end_head(canvas, tx + 1, tyc, edge.head_to, '◄', R);
    start_head(canvas, sx, sy, edge.head_from, '◄');
    if let Some(label) = &edge.label {
        place_label(canvas, label, tyc.saturating_sub(1), lane_x.saturating_sub(string_width(label) + 1));
    }
}

/// Rank to next rank, left to right: out the right side, jog on the bus column.
pub(super) fn route_forward_lr(canvas: &mut Canvas, from: &Placed, to: &Placed, edge: &Edge, bus: usize) {
    let (rx, ry) = (from.x + from.w - 1, from.cy);
    let ly = to.cy;
    let head_col = to.x - 1;

    canvas.junction(rx, ry, R);
    canvas.seg_h(ry, rx, bus);
    if ry == ly {
        canvas.seg_h(ry, bus, head_col);
    } else {
        canvas.seg_v(bus, ry, ly);
        canvas.seg_h(ly, bus, head_col);
    }
    end_head(canvas, head_col, ly, edge.head_to, '▶', R);
    start_head(canvas, rx, ry, edge.head_from, '◄');
    if let Some(label) = &edge.label {
        place_label(canvas, label, ly.saturating_sub(1), bus + 1);
    }
}

/// Skip or back edge, left to right: down out the bottom, along a lane, back up.
pub(super) fn route_back_lr(canvas: &mut Canvas, from: &Placed, to: &Placed, edge: &Edge, lane_y: usize) {
    let (sx, sy) = (from.cx, from.y + from.h - 1);
    let (tx, ty) = (to.cx, to.y + to.h - 1);

    canvas.junction(sx, sy, D);
    canvas.seg_v(sx, sy, lane_y);
    canvas.seg_h(lane_y, sx, tx);
    canvas.seg_v(tx, lane_y, ty + 1);
    end_head(canvas, tx, ty + 1, edge.head_to, '▲', D);
    start_head(canvas, sx, sy, edge.head_from, '▲');
    if let Some(label) = &edge.label {
        place_label(canvas, label, lane_y.saturating_sub(1), (sx + tx) / 2);
    }
}

/// Write an edge label, stopping at the first cell already in use.
fn place_label(canvas: &mut Canvas, label: &str, row: usize, start_x: usize) {
    if row >= canvas.h {
        return;
    }
    let text = fit_label(label, MAX_LABEL);
    let mut x = start_x;
    for (c, cw) in measured(&text) {
        if cw == 0 {
            continue;
        }
        if x + cw > canvas.w {
            break;
        }
        let blocked = (0..cw).any(|k| {
            let i = canvas.idx(x + k, row);
            !canvas.is_blank(i) || canvas.mask[i] != 0 || canvas.occupied[i]
        });
        if blocked {
            break;
        }
        canvas.set(x, row, c, Cls::EdgeLabel);
        for k in 1..cw {
            canvas.set_cont(x + k, row, Cls::EdgeLabel);
        }
        x += cw;
    }
}
