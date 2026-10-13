//! Word wrap for editor lines (upstream `wordWrapLine`).

use unicode_segmentation::UnicodeSegmentation;

use super::segment::{TextSegment, is_paste_marker};
use crate::utils::{CJK_BREAK_REGEX, is_whitespace_char, visible_width};

/// A wrapped piece of a line and its byte range in the line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextChunk {
    pub text: String,
    pub start_index: usize,
    pub end_index: usize,
}

/// Wraps `line` at word boundaries, breaking words wider than `max_width` by grapheme.
pub fn word_wrap_line(line: &str, max_width: usize) -> Vec<TextChunk> {
    word_wrap_line_with_segments(line, max_width, None)
}

fn chunk(line: &str, start: usize, end: usize) -> TextChunk {
    TextChunk { text: line[start..end].to_string(), start_index: start, end_index: end }
}

/// [`word_wrap_line`] with pre-computed segments (for example with paste markers merged).
pub fn word_wrap_line_with_segments(
    line: &str,
    max_width: usize,
    segments: Option<&[TextSegment<'_>]>,
) -> Vec<TextChunk> {
    if line.is_empty() || max_width == 0 {
        return vec![TextChunk { text: String::new(), start_index: 0, end_index: 0 }];
    }
    if visible_width(line) <= max_width {
        return vec![chunk(line, 0, line.len())];
    }

    let owned: Vec<TextSegment<'_>>;
    let segments = match segments {
        Some(segments) => segments,
        None => {
            owned = line.grapheme_indices(true).map(|(index, segment)| TextSegment { segment, index }).collect();
            &owned
        }
    };

    let mut chunks = Vec::new();
    let mut current_width = 0usize;
    let mut chunk_start = 0usize;
    // Where a break is allowed: after the last whitespace before a non-whitespace grapheme, or
    // next to CJK text. Holds the byte index and the width up to it.
    let mut wrap_opportunity: Option<(usize, usize)> = None;

    for (i, seg) in segments.iter().enumerate() {
        let grapheme = seg.segment;
        let grapheme_width = visible_width(grapheme);
        let char_index = seg.index;
        let is_whitespace = !is_paste_marker(grapheme) && is_whitespace_char(grapheme);

        if current_width + grapheme_width > max_width {
            match wrap_opportunity {
                Some((index, width)) if current_width - width + grapheme_width <= max_width => {
                    chunks.push(chunk(line, chunk_start, index));
                    chunk_start = index;
                    current_width -= width;
                }
                _ if chunk_start < char_index => {
                    chunks.push(chunk(line, chunk_start, char_index));
                    chunk_start = char_index;
                    current_width = 0;
                }
                _ => {}
            }
            wrap_opportunity = None;
        }

        // An atomic segment wider than the line (a paste marker in a narrow terminal) is split
        // visually by grapheme. A single grapheme cannot be split, so it overflows instead.
        if grapheme_width > max_width && grapheme.graphemes(true).nth(1).is_some() {
            let sub_chunks = word_wrap_line(grapheme, max_width);
            let Some((last, rest)) = sub_chunks.split_last() else { continue };
            for sub in rest {
                chunks.push(TextChunk {
                    text: sub.text.clone(),
                    start_index: char_index + sub.start_index,
                    end_index: char_index + sub.end_index,
                });
            }
            chunk_start = char_index + last.start_index;
            current_width = visible_width(&last.text);
            wrap_opportunity = None;
            continue;
        }

        current_width += grapheme_width;

        let Some(next) = segments.get(i + 1) else { continue };
        if is_whitespace && (is_paste_marker(next.segment) || !is_whitespace_char(next.segment)) {
            wrap_opportunity = Some((next.index, current_width));
        } else if !is_whitespace && !is_whitespace_char(next.segment) {
            let is_cjk = !is_paste_marker(grapheme) && CJK_BREAK_REGEX.is_match(grapheme);
            let next_is_cjk = !is_paste_marker(next.segment) && CJK_BREAK_REGEX.is_match(next.segment);
            if is_cjk || next_is_cjk {
                wrap_opportunity = Some((next.index, current_width));
            }
        }
    }

    chunks.push(chunk(line, chunk_start, line.len()));
    chunks
}
