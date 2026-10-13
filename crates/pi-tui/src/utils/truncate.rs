//! Width-limited truncation with an ellipsis.

use unicode_segmentation::UnicodeSegmentation;

use super::ansi::{extract_ansi_code, next_tab_or_ansi};
use super::is_printable_ascii;
use super::osc8::get_active_osc8_close;
use super::width::{grapheme_width, visible_width};

const RESET: &str = "\x1b[0m";

/// Keeps the longest prefix of `text` that fits in `max_width`; returns it with its width.
fn truncate_fragment_to_width(text: &str, max_width: usize) -> (String, usize) {
    if max_width == 0 || text.is_empty() {
        return (String::new(), 0);
    }
    if is_printable_ascii(text) {
        let clipped = &text[..max_width.min(text.len())];
        return (clipped.to_string(), clipped.len());
    }

    let mut result = String::new();
    let mut width = 0;
    if !text.contains(['\x1b', '\t']) {
        for segment in text.graphemes(true) {
            let w = grapheme_width(segment);
            if width + w > max_width {
                break;
            }
            result.push_str(segment);
            width += w;
        }
        return (result, width);
    }

    let mut pending_ansi = String::new();
    let mut i = 0;
    while i < text.len() {
        if let Some(code) = extract_ansi_code(text, i) {
            pending_ansi.push_str(code);
            i += code.len();
            continue;
        }
        if text.as_bytes()[i] == b'\t' {
            if width + 3 > max_width {
                break;
            }
            result.push_str(&pending_ansi);
            pending_ansi.clear();
            result.push('\t');
            width += 3;
            i += 1;
            continue;
        }
        let end = next_tab_or_ansi(text, i);
        for segment in text[i..end].graphemes(true) {
            let w = grapheme_width(segment);
            if width + w > max_width {
                return (result, width);
            }
            result.push_str(&pending_ansi);
            pending_ansi.clear();
            result.push_str(segment);
            width += w;
        }
        i = end;
    }
    (result, width)
}

fn finalize_truncated_result(
    prefix: &str,
    prefix_width: usize,
    ellipsis: &str,
    ellipsis_width: usize,
    max_width: usize,
    pad: bool,
) -> String {
    let hyperlink_close = get_active_osc8_close(prefix);
    let mut result = if ellipsis.is_empty() {
        format!("{prefix}{hyperlink_close}{RESET}")
    } else {
        format!("{prefix}{hyperlink_close}{RESET}{ellipsis}{RESET}")
    };
    if pad {
        result.push_str(&" ".repeat(max_width.saturating_sub(prefix_width + ellipsis_width)));
    }
    result
}

fn padded(text: &str, pad: bool, padding: usize) -> String {
    if pad { format!("{text}{}", " ".repeat(padding)) } else { text.to_string() }
}

/// Truncates `text` to `max_width` cells with a `"..."` ellipsis and no padding.
pub fn truncate_to_width(text: &str, max_width: usize) -> String {
    truncate_to_width_with(text, max_width, "...", false)
}

/// Truncates `text` to `max_width` cells (escape codes are free), appending `ellipsis` when text was cut.
/// A cut closes any open OSC 8 link and resets SGR around the ellipsis. `pad` fills up to `max_width`.
pub fn truncate_to_width_with(text: &str, max_width: usize, ellipsis: &str, pad: bool) -> String {
    if max_width == 0 {
        return String::new();
    }
    if text.is_empty() {
        return padded("", pad, max_width);
    }

    let ellipsis_width = visible_width(ellipsis);
    if ellipsis_width >= max_width {
        let text_width = visible_width(text);
        if text_width <= max_width {
            return padded(text, pad, max_width - text_width);
        }
        let (clipped, clipped_width) = truncate_fragment_to_width(ellipsis, max_width);
        if clipped_width == 0 {
            return padded("", pad, max_width);
        }
        return finalize_truncated_result("", 0, &clipped, clipped_width, max_width, pad);
    }

    let target_width = max_width - ellipsis_width;
    if is_printable_ascii(text) {
        if text.len() <= max_width {
            return padded(text, pad, max_width - text.len());
        }
        return finalize_truncated_result(
            &text[..target_width],
            target_width,
            ellipsis,
            ellipsis_width,
            max_width,
            pad,
        );
    }

    let mut state = PrefixState { result: String::new(), kept_width: 0, keep_prefix: true, visible: 0, target_width };
    let mut pending_ansi = String::new();
    // When nothing overflows, the whole input was consumed and fits.
    let mut overflowed = false;

    if !text.contains(['\x1b', '\t']) {
        for segment in text.graphemes(true) {
            state.offer(segment, grapheme_width(segment), &mut pending_ansi);
            if state.visible > max_width {
                overflowed = true;
                break;
            }
        }
    } else {
        let mut i = 0;
        while i < text.len() {
            if let Some(code) = extract_ansi_code(text, i) {
                pending_ansi.push_str(code);
                i += code.len();
                continue;
            }
            if text.as_bytes()[i] == b'\t' {
                state.offer("\t", 3, &mut pending_ansi);
                if state.visible > max_width {
                    overflowed = true;
                    break;
                }
                i += 1;
                continue;
            }
            let end = next_tab_or_ansi(text, i);
            for segment in text[i..end].graphemes(true) {
                state.offer(segment, grapheme_width(segment), &mut pending_ansi);
                if state.visible > max_width {
                    overflowed = true;
                    break;
                }
            }
            if overflowed {
                break;
            }
            i = end;
        }
    }

    if !overflowed {
        return padded(text, pad, max_width.saturating_sub(state.visible));
    }
    finalize_truncated_result(&state.result, state.kept_width, ellipsis, ellipsis_width, max_width, pad)
}

/// Accumulates the contiguous prefix that fits before the ellipsis.
struct PrefixState {
    result: String,
    kept_width: usize,
    keep_prefix: bool,
    visible: usize,
    target_width: usize,
}

impl PrefixState {
    fn offer(&mut self, segment: &str, width: usize, pending_ansi: &mut String) {
        if self.keep_prefix && self.kept_width + width <= self.target_width {
            self.result.push_str(pending_ansi);
            self.result.push_str(segment);
            self.kept_width += width;
        } else {
            self.keep_prefix = false;
        }
        pending_ansi.clear();
        self.visible += width;
    }
}
