//! Paste markers (`[paste #1 +123 lines]`) and segmentation that keeps a marker as one unit.

use std::sync::LazyLock;

use regex::Regex;
use unicode_segmentation::UnicodeSegmentation;

use crate::utils::{WordSegment, word_segments};

/// Any paste marker, with the id in group 1 and the size suffix in group 2.
pub(super) static PASTE_MARKER_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\[paste #(\d+)( (\+\d+ lines|\d+ chars))?\]").unwrap_or_else(|err| panic!("paste marker regex: {err}"))
});

static PASTE_MARKER_SINGLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\[paste #(\d+)( (\+\d+ lines|\d+ chars))?\]$")
        .unwrap_or_else(|err| panic!("paste marker regex: {err}"))
});

/// True when `segment` is exactly one paste marker.
pub(super) fn is_paste_marker(segment: &str) -> bool {
    segment.len() >= 10 && PASTE_MARKER_SINGLE.is_match(segment)
}

/// The id of a segment that is exactly one paste marker.
pub(super) fn paste_marker_id(segment: &str) -> Option<u32> {
    PASTE_MARKER_SINGLE.captures(segment).and_then(|c| c[1].parse().ok())
}

/// One grapheme (or merged paste marker) and its byte offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextSegment<'a> {
    pub segment: &'a str,
    pub index: usize,
}

/// Pasted content by id, in insertion order like a JavaScript `Map`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct PasteRegistry {
    entries: Vec<(u32, String)>,
}

impl PasteRegistry {
    pub fn get(&self, id: u32) -> Option<&str> {
        self.entries.iter().find(|(key, _)| *key == id).map(|(_, value)| value.as_str())
    }

    pub fn contains(&self, id: u32) -> bool {
        self.get(id).is_some()
    }

    /// Updates an existing id in place or appends a new one.
    pub fn set(&mut self, id: u32, content: String) {
        match self.entries.iter_mut().find(|(key, _)| *key == id) {
            Some((_, value)) => *value = content,
            None => self.entries.push((id, content)),
        }
    }

    pub fn remove(&mut self, id: u32) {
        self.entries.retain(|(key, _)| *key != id);
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn ids(&self) -> Vec<u32> {
        self.entries.iter().map(|(id, _)| *id).collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = (u32, &str)> {
        self.entries.iter().map(|(id, value)| (*id, value.as_str()))
    }
}

/// Byte spans of markers whose id is registered.
fn marker_spans(text: &str, registry: &PasteRegistry) -> Vec<(usize, usize)> {
    if registry.is_empty() || !text.contains("[paste #") {
        return Vec::new();
    }
    PASTE_MARKER_REGEX
        .captures_iter(text)
        .filter(|c| c[1].parse::<u32>().is_ok_and(|id| registry.contains(id)))
        .filter_map(|c| c.get(0))
        .map(|m| (m.start(), m.end()))
        .collect()
}

/// Replaces base segments that start inside a marker with one segment for the whole marker.
fn merge_markers<T>(
    base: Vec<T>,
    markers: &[(usize, usize)],
    index_of: impl Fn(&T) -> usize,
    make: impl Fn(usize, usize) -> T,
) -> Vec<T> {
    if markers.is_empty() {
        return base;
    }
    let mut result = Vec::with_capacity(base.len());
    let mut marker_index = 0;
    for segment in base {
        let index = index_of(&segment);
        while marker_index < markers.len() && markers[marker_index].1 <= index {
            marker_index += 1;
        }
        match markers.get(marker_index) {
            Some(&(start, end)) if index >= start && index < end => {
                if index == start {
                    result.push(make(start, end));
                }
            }
            _ => result.push(segment),
        }
    }
    result
}

/// Graphemes of `text` with registered paste markers merged.
pub(super) fn grapheme_segments<'a>(text: &'a str, registry: &PasteRegistry) -> Vec<TextSegment<'a>> {
    let base: Vec<TextSegment<'a>> =
        text.grapheme_indices(true).map(|(index, segment)| TextSegment { segment, index }).collect();
    let markers = marker_spans(text, registry);
    merge_markers(base, &markers, |s| s.index, |start, end| TextSegment { segment: &text[start..end], index: start })
}

/// Word segments of `text` with registered paste markers merged.
pub(super) fn word_segments_with_markers<'a>(text: &'a str, registry: &PasteRegistry) -> Vec<WordSegment<'a>> {
    let markers = marker_spans(text, registry);
    merge_markers(
        word_segments(text),
        &markers,
        |s| s.index,
        |start, end| WordSegment { segment: &text[start..end], index: start, is_word_like: false },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_only_registered_markers() {
        let mut registry = PasteRegistry::default();
        registry.set(1, "content".to_string());
        let text = "a[paste #1 +12 lines]b[paste #2 5 chars]";
        let segments: Vec<&str> = grapheme_segments(text, &registry).iter().map(|s| s.segment).collect();
        assert_eq!(segments[..3], ["a", "[paste #1 +12 lines]", "b"]);
        assert_eq!(segments[3], "[");
        assert!(is_paste_marker("[paste #1 +12 lines]"));
        assert!(!is_paste_marker("[paste #]"));
        assert_eq!(paste_marker_id("[paste #12 34 chars]"), Some(12));
    }

    #[test]
    fn registry_keeps_insertion_order() {
        let mut registry = PasteRegistry::default();
        registry.set(2, "b".to_string());
        registry.set(1, "a".to_string());
        registry.set(2, "c".to_string());
        assert_eq!(registry.ids(), [2, 1]);
        registry.remove(2);
        registry.set(2, "d".to_string());
        assert_eq!(registry.ids(), [1, 2]);
        assert_eq!(registry.get(2), Some("d"));
    }
}
