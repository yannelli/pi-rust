//! The character grid every layout paints onto (upstream `canvas.ts`). Edges add
//! direction bits rather than glyphs, so crossings resolve in any drawing order.

use super::types::{Cls, Span};
use super::width::measured;

/// Occupies the trailing column of a wide glyph; skipped when building lines.
pub(super) const CONT: u32 = 0;
const SPACE: u32 = ' ' as u32;
/// Cell codes at or above this index a multi-code-point cluster in `clusters`.
const CLUSTER_BASE: u32 = 0x11_0000;

/// Connection direction bits, combined into a box-drawing glyph by `mask_char`.
pub(super) const U: u8 = 1;
pub(super) const D: u8 = 2;
pub(super) const L: u8 = 4;
pub(super) const R: u8 = 8;

/// Line styles, tracked per cell so crossing edges keep their own stroke.
pub(super) const STY_DOT: u8 = 1;
pub(super) const STY_THICK: u8 = 2;
pub(super) const STY_SOLID: u8 = 4;

pub(super) struct Canvas {
    pub w: usize,
    pub h: usize,
    /// A char code, `CONT`, or `CLUSTER_BASE + i` for `clusters[i]`.
    pub ch: Vec<u32>,
    pub cls: Vec<Cls>,
    pub mask: Vec<u8>,
    pub style: Vec<u8>,
    /// Cells claimed by a box, which edge bits must not overwrite.
    pub occupied: Vec<bool>,
    pub cur_style: u8,
    clusters: Vec<String>,
}

impl Canvas {
    pub(super) fn new(w: usize, h: usize) -> Self {
        let n = w * h;
        Canvas {
            w,
            h,
            ch: vec![SPACE; n],
            cls: vec![Cls::None; n],
            mask: vec![0; n],
            style: vec![0; n],
            occupied: vec![false; n],
            cur_style: STY_SOLID,
            clusters: Vec::new(),
        }
    }

    pub(super) fn idx(&self, x: usize, y: usize) -> usize {
        y * self.w + x
    }

    fn intern(&mut self, glyph: &str) -> u32 {
        let mut chars = glyph.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => u32::from(c),
            _ => {
                self.clusters.push(glyph.to_string());
                CLUSTER_BASE + (self.clusters.len() - 1) as u32
            }
        }
    }

    pub(super) fn is_blank(&self, i: usize) -> bool {
        self.ch[i] == SPACE
    }

    fn put(&mut self, x: usize, y: usize, code: u32, cls: Cls) {
        if x >= self.w || y >= self.h {
            return;
        }
        let i = self.idx(x, y);
        self.ch[i] = code;
        self.cls[i] = cls;
    }

    /// Paint one grapheme cluster.
    pub(super) fn set(&mut self, x: usize, y: usize, glyph: &str, cls: Cls) {
        if x >= self.w || y >= self.h {
            return;
        }
        let code = self.intern(glyph);
        self.put(x, y, code, cls);
    }

    pub(super) fn set_char(&mut self, x: usize, y: usize, c: char, cls: Cls) {
        self.put(x, y, u32::from(c), cls);
    }

    pub(super) fn set_cont(&mut self, x: usize, y: usize, cls: Cls) {
        self.put(x, y, CONT, cls);
    }

    /// Accumulate direction bits on a free cell. `border` cells keep their class, so a
    /// connector meeting a box keeps the box's styling.
    pub(super) fn add_bits(&mut self, x: usize, y: usize, bits: u8, cls: Cls) {
        if x >= self.w || y >= self.h {
            return;
        }
        let i = self.idx(x, y);
        if self.occupied[i] {
            return;
        }
        self.mask[i] |= bits;
        self.style[i] |= self.cur_style;
        if self.cls[i] != Cls::Border {
            self.cls[i] = cls;
        }
    }

    /// Stamp a finished sub-canvas (a subgraph frame's contents) at an offset.
    pub(super) fn blit(&mut self, sub: &Canvas, ox: usize, oy: usize) {
        for sy in 0..sub.h {
            for sx in 0..sub.w {
                let (x, y) = (ox + sx, oy + sy);
                if x >= self.w || y >= self.h {
                    continue;
                }
                let si = sub.idx(sx, sy);
                let di = self.idx(x, y);
                let code = sub.ch[si];
                self.ch[di] = match code.checked_sub(CLUSTER_BASE) {
                    Some(k) => self.intern(&sub.clusters[k as usize]),
                    None => code,
                };
                self.cls[di] = sub.cls[si];
                self.style[di] = sub.style[si];
                self.occupied[di] = true;
            }
        }
    }

    /// Add direction bits even to an occupied cell, so an edge can meet a border.
    pub(super) fn junction(&mut self, x: usize, y: usize, bits: u8) {
        if x >= self.w || y >= self.h {
            return;
        }
        let i = self.idx(x, y);
        self.mask[i] |= bits;
        if self.cls[i] != Cls::Border {
            self.cls[i] = Cls::Edge;
        }
    }

    pub(super) fn seg_v(&mut self, x: usize, y0: usize, y1: usize) {
        let (a, b) = (y0.min(y1), y0.max(y1));
        for y in a..=b {
            let bits = if y > a { U } else { 0 } | if y < b { D } else { 0 };
            self.add_bits(x, y, bits, Cls::Edge);
        }
    }

    pub(super) fn seg_h(&mut self, y: usize, x0: usize, x1: usize) {
        let (a, b) = (x0.min(x1), x0.max(x1));
        for x in a..=b {
            let bits = if x > a { L } else { 0 } | if x < b { R } else { 0 };
            self.add_bits(x, y, bits, Cls::Edge);
        }
    }

    /// Resolve accumulated direction bits into glyphs, honouring line style.
    pub(super) fn finalize_mask(&mut self) {
        for i in 0..self.ch.len() {
            if self.mask[i] != 0 && self.ch[i] == SPACE {
                let c = mask_char(self.mask[i]);
                let c = match self.style[i] {
                    STY_DOT => dotted_char(c),
                    STY_THICK => thick_char(c),
                    _ => c,
                };
                self.ch[i] = u32::from(c);
            }
        }
    }

    fn map_glyphs(&mut self, flip: fn(char) -> char) {
        for code in &mut self.ch {
            if let Some(c) = char::from_u32(*code) {
                *code = u32::from(flip(c));
            }
        }
    }

    /// Mirror top to bottom for `BT`. Rows reorder but text within a row does not.
    pub(super) fn flip_vertical(&mut self) {
        for y in 0..self.h / 2 {
            let y2 = self.h - 1 - y;
            for x in 0..self.w {
                let (i, j) = (self.idx(x, y), self.idx(x, y2));
                self.ch.swap(i, j);
                self.cls.swap(i, j);
            }
        }
        self.map_glyphs(flip_glyph_v);
    }

    /// Mirror left to right for `RL`, then reverse each text or label run back into
    /// reading order.
    pub(super) fn flip_horizontal(&mut self) {
        for y in 0..self.h {
            for x in 0..self.w / 2 {
                let (i, j) = (self.idx(x, y), self.idx(self.w - 1 - x, y));
                self.ch.swap(i, j);
                self.cls.swap(i, j);
            }
        }
        self.map_glyphs(flip_glyph_h);
        for y in 0..self.h {
            let mut x = 0;
            while x < self.w {
                let cls = self.cls[self.idx(x, y)];
                if cls == Cls::Text || cls == Cls::EdgeLabel {
                    let start = self.idx(x, y);
                    while x < self.w && self.cls[self.idx(x, y)] == cls {
                        x += 1;
                    }
                    let end = self.idx(x, y);
                    self.ch[start..end].reverse();
                } else {
                    x += 1;
                }
            }
        }
    }

    fn push_glyph(&self, out: &mut String, code: u32) {
        match code.checked_sub(CLUSTER_BASE) {
            Some(k) => out.push_str(&self.clusters[k as usize]),
            None => out.extend(char::from_u32(code)),
        }
    }

    /// Group each row into runs of one class, dropping wide-glyph continuations and
    /// blank rows at the top and bottom. Returns `(plain, styled, width)`.
    pub(super) fn to_lines(&self) -> (Vec<String>, Vec<Vec<Span>>, usize) {
        let mut plain = Vec::with_capacity(self.h);
        let mut styled = Vec::with_capacity(self.h);
        let mut width = 0;
        for y in 0..self.h {
            // A trailing CONT counts as painted: the wide glyph reaches that column.
            let last = (0..self.w).rev().find(|&x| !self.is_blank(self.idx(x, y))).map_or(0, |x| x + 1);
            width = width.max(last);
            let mut spans = Vec::new();
            let mut plain_row = String::new();
            let mut run = String::new();
            let mut run_cls = Cls::None;
            for x in 0..last {
                let i = self.idx(x, y);
                let code = self.ch[i];
                if code == CONT {
                    continue;
                }
                let cls = self.cls[i];
                self.push_glyph(&mut plain_row, code);
                if cls != run_cls && !run.is_empty() {
                    spans.push(Span { text: std::mem::take(&mut run), cls: run_cls });
                }
                run_cls = cls;
                self.push_glyph(&mut run, code);
            }
            if !run.is_empty() {
                spans.push(Span { text: run, cls: run_cls });
            }
            styled.push(spans);
            // Only ASCII spaces: trimming all whitespace would desync `plain` and `styled`.
            plain.push(plain_row.trim_end_matches(' ').to_string());
        }
        let first = plain.iter().position(|row| !row.is_empty()).unwrap_or(plain.len());
        let end = plain.iter().rposition(|row| !row.is_empty()).map_or(first, |i| i + 1);
        plain.truncate(end);
        styled.truncate(end);
        plain.drain(..first);
        styled.drain(..first);
        (plain, styled, width)
    }
}

/// Paint `text` at `x, y`, one grapheme cluster per cell. A wide cluster claims a
/// second cell marked `CONT`; a zero-width cluster is skipped.
pub(super) fn draw_text(canvas: &mut Canvas, text: &str, x: usize, y: usize, cls: Cls) {
    let mut cur = x;
    for (cluster, cw) in measured(text) {
        if cw == 0 {
            continue;
        }
        canvas.set(cur, y, cluster, cls);
        for k in 1..cw {
            canvas.set_cont(cur + k, y, cls);
        }
        cur += cw;
    }
}

/// Paint `text` at `x, y`, clearing any edge bits underneath first so it wins over a
/// line drawn through the same row.
pub(super) fn draw_text_over_edges(canvas: &mut Canvas, text: &str, x: usize, y: usize, cls: Cls) {
    let mut cur = x;
    for (cluster, cw) in measured(text) {
        if cw == 0 {
            continue;
        }
        for k in 0..cw {
            if cur + k < canvas.w && y < canvas.h {
                let i = canvas.idx(cur + k, y);
                canvas.mask[i] = 0;
            }
            if k == 0 {
                canvas.set(cur, y, cluster, cls);
            } else {
                canvas.set_cont(cur + k, y, cls);
            }
        }
        cur += cw;
    }
}

/// Box-drawing glyph for a set of `U`/`D`/`L`/`R` bits.
pub(super) fn mask_char(mask: u8) -> char {
    match mask {
        0 => ' ',
        1..=3 => '│',
        4 | 8 | 12 => '─',
        10 => '┌',
        6 => '┐',
        9 => '└',
        5 => '┘',
        11 => '├',
        7 => '┤',
        14 => '┬',
        13 => '┴',
        _ => '┼',
    }
}

fn dotted_char(c: char) -> char {
    match c {
        '─' => '╌',
        '│' => '╎',
        _ => c,
    }
}

fn thick_char(c: char) -> char {
    match c {
        '─' => '━',
        '│' => '┃',
        '┌' => '┏',
        '┐' => '┓',
        '└' => '┗',
        '┘' => '┛',
        '├' => '┣',
        '┤' => '┫',
        '┬' => '┳',
        '┴' => '┻',
        '┼' => '╋',
        _ => c,
    }
}

fn flip_glyph_v(c: char) -> char {
    const PAIRS: [(char, char); 10] = [
        ('┌', '└'),
        ('┐', '┘'),
        ('┏', '┗'),
        ('┓', '┛'),
        ('╭', '╰'),
        ('╮', '╯'),
        ('┬', '┴'),
        ('┳', '┻'),
        ('▼', '▲'),
        ('▽', '△'),
    ];
    swap_pair(&PAIRS, c)
}

fn flip_glyph_h(c: char) -> char {
    const PAIRS: [(char, char); 10] = [
        ('┌', '┐'),
        ('└', '┘'),
        ('┏', '┓'),
        ('┗', '┛'),
        ('╭', '╮'),
        ('╰', '╯'),
        ('├', '┤'),
        ('┣', '┫'),
        ('▶', '◄'),
        ('▷', '◁'),
    ];
    swap_pair(&PAIRS, c)
}

fn swap_pair(pairs: &[(char, char)], c: char) -> char {
    for &(a, b) in pairs {
        if c == a {
            return b;
        }
        if c == b {
            return a;
        }
    }
    c
}
