//! Column-based slicing of styled lines.

use unicode_segmentation::UnicodeSegmentation;

use super::ansi::{extract_ansi_code, next_ansi_start};
use super::tracker::AnsiCodeTracker;
use super::width::grapheme_width;

/// The terminal cells `start..end` taken by one grapheme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphemeCellRange {
    /// First cell of the grapheme.
    pub start: usize,
    /// One past the last cell of the grapheme.
    pub end: usize,
}

/// Text cut from a line, with its visible width.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SlicedText {
    /// The sliced text, including escape codes inside the range.
    pub text: String,
    /// Visible width of `text` in cells.
    pub width: usize,
}

/// Content before and after an overlay region of a line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LineSegments {
    /// Content left of the overlay.
    pub before: String,
    /// Visible width of `before`.
    pub before_width: usize,
    /// Content right of the overlay, prefixed with the styling active at its start.
    pub after: String,
    /// Visible width of `after`.
    pub after_width: usize,
}

/// A piece of a styled line: an escape code or a visible grapheme, with the column where it starts.
enum Piece<'a> {
    Ansi(&'a str, usize),
    Text(&'a str, usize, usize),
}

/// Walks escape codes and graphemes in order; `handle` returns `true` to stop after a grapheme.
fn walk_line<'a>(line: &'a str, mut handle: impl FnMut(Piece<'a>) -> bool) {
    let mut current_col = 0;
    let mut i = 0;
    while i < line.len() {
        if let Some(code) = extract_ansi_code(line, i) {
            handle(Piece::Ansi(code, current_col));
            i += code.len();
            continue;
        }
        let text_end = next_ansi_start(line, i);
        for segment in line[i..text_end].graphemes(true) {
            let width = grapheme_width(segment);
            if handle(Piece::Text(segment, current_col, width)) {
                return;
            }
            current_col += width;
        }
        i = text_end;
    }
}

/// Returns the cell range of the grapheme drawn at a visible column, if the column has one.
pub fn get_grapheme_cell_range(line: &str, column: usize) -> Option<GraphemeCellRange> {
    let mut found = None;
    walk_line(line, |piece| match piece {
        Piece::Text(_, col, width) if width > 0 && column >= col && column < col + width => {
            found = Some(GraphemeCellRange { start: col, end: col + width });
            true
        }
        _ => false,
    });
    found
}

/// Extracts `length` visible columns starting at `start_col`, keeping escape codes and wide characters.
/// With `strict`, a wide character that would extend past the range is left out.
pub fn slice_by_column(line: &str, start_col: usize, length: usize, strict: bool) -> String {
    slice_with_width(line, start_col, length, strict).text
}

/// Like [`slice_by_column`], and also returns the visible width of the result.
pub fn slice_with_width(line: &str, start_col: usize, length: usize, strict: bool) -> SlicedText {
    if length == 0 {
        return SlicedText::default();
    }
    let end_col = start_col.saturating_add(length);
    let mut result = String::new();
    let mut result_width = 0;
    let mut pending_ansi = String::new();
    walk_line(line, |piece| match piece {
        Piece::Ansi(code, col) => {
            if col >= start_col && col < end_col {
                // Codes from before the range must precede codes at the boundary.
                result.push_str(&pending_ansi);
                pending_ansi.clear();
                result.push_str(code);
            } else if col < start_col {
                pending_ansi.push_str(code);
            }
            false
        }
        Piece::Text(segment, col, width) => {
            let in_range = col >= start_col && col < end_col;
            let fits = !strict || col + width <= end_col;
            if in_range && fits {
                result.push_str(&pending_ansi);
                pending_ansi.clear();
                result.push_str(segment);
                result_width += width;
            }
            col + width >= end_col
        }
    });
    SlicedText { text: result, width: result_width }
}

/// Splits a line around an overlay in one pass: `before` is columns `0..before_end`, `after` is
/// `after_len` columns from `after_start`. `after` starts with the styling active before the overlay.
pub fn extract_segments(
    line: &str,
    before_end: usize,
    after_start: usize,
    after_len: usize,
    strict_after: bool,
) -> LineSegments {
    let mut out = LineSegments::default();
    let mut pending_ansi_before = String::new();
    let mut after_started = false;
    let after_end = after_start.saturating_add(after_len);
    let stop_col = if after_len == 0 { before_end } else { after_end };
    let mut tracker = AnsiCodeTracker::new();

    walk_line(line, |piece| match piece {
        Piece::Ansi(code, col) => {
            tracker.process(code);
            if col < before_end {
                pending_ansi_before.push_str(code);
            } else if col >= after_start && col < after_end && (after_started || code.starts_with("\x1b_")) {
                // SGR codes join once `after` has started; APC markers such as the cursor marker always do.
                out.after.push_str(code);
            }
            false
        }
        Piece::Text(segment, col, width) => {
            if col < before_end && col + width <= before_end {
                out.before.push_str(&pending_ansi_before);
                pending_ansi_before.clear();
                out.before.push_str(segment);
                out.before_width += width;
            } else if col >= after_start && col < after_end && (!strict_after || col + width <= after_end) {
                if !after_started {
                    out.after.push_str(&tracker.get_active_codes());
                    after_started = true;
                }
                out.after.push_str(segment);
                out.after_width += width;
            }
            col + width >= stop_col
        }
    });
    out
}
