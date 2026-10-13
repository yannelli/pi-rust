//! Word-wise cursor movement. Cursors are UTF-8 byte offsets.

use crate::utils::{PUNCTUATION_REGEX, WordSegment, is_whitespace_char, word_segments};

/// Custom segmenter: returns word segments that are slices of the given text.
pub type SegmentFn<'f> = Box<dyn for<'a> Fn(&'a str) -> Vec<WordSegment<'a>> + 'f>;

/// Predicate for segments that move as one unit (for example paste markers).
pub type AtomicSegmentFn<'f> = Box<dyn Fn(&str) -> bool + 'f>;

/// Options for [`find_word_backward`] and [`find_word_forward`]. The default uses [`word_segments`].
#[derive(Default)]
pub struct WordNavigationOptions<'f> {
    /// Custom segmenter used instead of [`word_segments`].
    pub segment: Option<SegmentFn<'f>>,
    /// Identifies segments that are skipped as a single unit.
    pub is_atomic_segment: Option<AtomicSegmentFn<'f>>,
}

impl<'f> WordNavigationOptions<'f> {
    /// Sets the custom segmenter.
    pub fn with_segment(mut self, segment: impl for<'a> Fn(&'a str) -> Vec<WordSegment<'a>> + 'f) -> Self {
        self.segment = Some(Box::new(segment));
        self
    }

    /// Sets the atomic segment predicate.
    pub fn with_atomic_segment(mut self, is_atomic: impl Fn(&str) -> bool + 'f) -> Self {
        self.is_atomic_segment = Some(Box::new(is_atomic));
        self
    }

    fn segments<'a>(&self, text: &'a str) -> Vec<WordSegment<'a>> {
        match &self.segment {
            Some(segment) => segment(text),
            None => word_segments(text),
        }
    }

    fn is_atomic(&self, segment: &str) -> bool {
        self.is_atomic_segment.as_ref().is_some_and(|is_atomic| is_atomic(segment))
    }
}

fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Cursor position after moving one word backward: skips trailing whitespace, then stops at the
/// next word or punctuation boundary.
pub fn find_word_backward(text: &str, cursor: usize, options: &WordNavigationOptions) -> usize {
    if cursor == 0 {
        return 0;
    }
    let cursor = floor_char_boundary(text, cursor);
    let mut segments = options.segments(&text[..cursor]);
    let mut new_cursor = cursor;

    while let Some(last) = segments.last() {
        if options.is_atomic(last.segment) || !is_whitespace_char(last.segment) {
            break;
        }
        new_cursor = new_cursor.saturating_sub(last.segment.len());
        segments.pop();
    }

    let Some(last) = segments.last() else {
        return new_cursor;
    };
    if options.is_atomic(last.segment) {
        new_cursor.saturating_sub(last.segment.len())
    } else if last.is_word_like {
        // Move within the word, stopping after its last ASCII punctuation character.
        let segment = last.segment;
        let keep = PUNCTUATION_REGEX.find_iter(segment).last().map_or(0, |m| m.end());
        new_cursor.saturating_sub(segment.len() - keep)
    } else {
        // Skip a run of punctuation and symbols.
        while let Some(last) = segments.last() {
            if options.is_atomic(last.segment) || last.is_word_like || is_whitespace_char(last.segment) {
                break;
            }
            new_cursor = new_cursor.saturating_sub(last.segment.len());
            segments.pop();
        }
        new_cursor
    }
}

/// Cursor position after moving one word forward: skips leading whitespace, then stops at the
/// next word or punctuation boundary.
pub fn find_word_forward(text: &str, cursor: usize, options: &WordNavigationOptions) -> usize {
    if cursor >= text.len() {
        return text.len();
    }
    let cursor = floor_char_boundary(text, cursor);
    let segments = options.segments(&text[cursor..]);
    let mut iter = segments.iter().peekable();
    let mut new_cursor = cursor;

    while let Some(next) = iter.next_if(|s| !options.is_atomic(s.segment) && is_whitespace_char(s.segment)) {
        new_cursor += next.segment.len();
    }

    let Some(next) = iter.peek() else {
        return new_cursor;
    };
    if options.is_atomic(next.segment) {
        new_cursor + next.segment.len()
    } else if next.is_word_like {
        // Move within the word, stopping before its first ASCII punctuation character.
        new_cursor + PUNCTUATION_REGEX.find(next.segment).map_or(next.segment.len(), |m| m.start())
    } else {
        // Skip a run of punctuation and symbols.
        while let Some(next) =
            iter.next_if(|s| !options.is_atomic(s.segment) && !s.is_word_like && !is_whitespace_char(s.segment))
        {
            new_cursor += next.segment.len();
        }
        new_cursor
    }
}
