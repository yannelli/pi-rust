//! Terminal cell width of graphemes and strings.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};

use unicode_segmentation::UnicodeSegmentation;

use super::ansi::ansi_code_length;
use super::char_class_table::CHAR_CLASS_RANGES;
use super::eaw_table::{FULLWIDTH_RANGES, WIDE_RANGES};
use super::emoji::{could_be_emoji, is_rgi_emoji};

/// Default_Ignorable_Code_Point, Control, or Mark.
const ZERO_WIDTH: u8 = 1;
/// Default_Ignorable_Code_Point, Control, Format, or Mark.
const NON_PRINTING: u8 = 2;
/// General category Mark.
const MARK: u8 = 4;
/// Marks that terminals allocate a cell for (spacing marks plus legacy wcwidth exceptions).
const SPACING_MARK: u8 = 8;

const WIDTH_CACHE_SIZE: usize = 512;

fn char_class(c: char) -> u8 {
    let cp = c as u32;
    let idx = CHAR_CLASS_RANGES.partition_point(|&(_, end, _)| end < cp);
    match CHAR_CLASS_RANGES.get(idx) {
        Some(&(start, _, flags)) if start <= cp => flags,
        _ => 0,
    }
}

/// Marks, format characters, and other default-ignorable code points; controls excluded.
pub(crate) fn is_extend_or_format(c: char) -> bool {
    char_class(c) & NON_PRINTING != 0 && !c.is_control()
}

pub(crate) fn in_ranges(ranges: &[(u32, u32)], cp: u32) -> bool {
    let idx = ranges.partition_point(|&(_, end)| end < cp);
    ranges.get(idx).is_some_and(|&(start, _)| start <= cp)
}

/// East Asian Width of a code point as a cell count: 2 for Wide and Fullwidth, otherwise 1.
pub fn east_asian_width(c: char) -> usize {
    let cp = c as u32;
    if in_ranges(FULLWIDTH_RANGES, cp) || in_ranges(WIDE_RANGES, cp) { 2 } else { 1 }
}

fn all_have(s: &str, flag: u8) -> bool {
    !s.is_empty() && s.chars().all(|c| char_class(c) & flag != 0)
}

/// Terminal width of one grapheme cluster.
pub fn grapheme_width(grapheme: &str) -> usize {
    if let [byte] = grapheme.as_bytes() {
        match byte {
            0x20..=0x7e => return 1,
            0x09 => return 3,
            _ => {}
        }
    }

    // Some marks occupy cells even without a base character.
    if all_have(grapheme, SPACING_MARK) {
        return grapheme.chars().count();
    }
    if all_have(grapheme, ZERO_WIDTH) {
        return 0;
    }
    if could_be_emoji(grapheme) && is_rgi_emoji(grapheme) {
        return 2;
    }

    let base = grapheme.trim_start_matches(|c| char_class(c) & NON_PRINTING != 0);
    let mut chars = base.chars();
    let Some(first) = chars.next() else {
        return 0;
    };

    // Regional indicators render full width even when a flag is incomplete (for example while streaming).
    if ('\u{1F1E6}'..='\u{1F1FF}').contains(&first) {
        return 2;
    }

    let mut width = east_asian_width(first);
    // Count trailing visible code points that terminals give cells: Indic consonants after marks,
    // halfwidth and fullwidth forms, and Thai/Lao AM vowels.
    let mut follows_mark = false;
    for c in chars {
        let class = char_class(c);
        if class & SPACING_MARK != 0 {
            width += 1;
            follows_mark = false;
        } else if class & MARK != 0 {
            follows_mark = true;
        } else if class & NON_PRINTING == 0 {
            if follows_mark || ('\u{FF00}'..='\u{FFEF}').contains(&c) {
                width += east_asian_width(c);
            } else if c == '\u{0E33}' || c == '\u{0EB3}' {
                width += 1;
            }
            follows_mark = false;
        }
    }
    width
}

/// Width of a string of printable ASCII, tabs, and escape sequences, or `None` if it has anything else.
fn ascii_visible_width(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut width = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            0x20..=0x7e => {
                width += 1;
                i += 1;
            }
            0x09 => {
                width += 3;
                i += 1;
            }
            0x1b => match ansi_code_length(s, i) {
                0 => return None,
                length => i += length,
            },
            _ => return None,
        }
    }
    Some(width)
}

#[derive(Default)]
struct WidthCache {
    map: HashMap<String, usize>,
    order: VecDeque<String>,
}

thread_local! {
    static WIDTH_CACHE: RefCell<WidthCache> = RefCell::new(WidthCache::default());
}

fn strip_for_width(s: &str) -> String {
    let mut clean = if s.contains('\t') { s.replace('\t', "   ") } else { s.to_string() };
    if !clean.contains('\x1b') {
        return clean;
    }
    let mut stripped = String::with_capacity(clean.len());
    let mut copy_from = 0;
    let mut search = 0;
    while let Some(offset) = clean[search..].find('\x1b') {
        let escape = search + offset;
        let length = ansi_code_length(&clean, escape);
        if length > 0 {
            stripped.push_str(&clean[copy_from..escape]);
            copy_from = escape + length;
            search = copy_from;
        } else {
            search = escape + 1;
        }
    }
    stripped.push_str(&clean[copy_from..]);
    clean = stripped;
    clean
}

/// Visible width of a string in terminal cells. Escape sequences are zero width and tabs count as 3.
pub fn visible_width(s: &str) -> usize {
    if s.is_empty() {
        return 0;
    }
    // Styled ASCII lines take this path, so a theme change does not segment every line again.
    if let Some(width) = ascii_visible_width(s) {
        return width;
    }
    if let Some(width) = WIDTH_CACHE.with(|cache| cache.borrow().map.get(s).copied()) {
        return width;
    }

    let width = strip_for_width(s).graphemes(true).map(grapheme_width).sum();

    WIDTH_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.map.len() >= WIDTH_CACHE_SIZE
            && let Some(oldest) = cache.order.pop_front()
        {
            cache.map.remove(&oldest);
        }
        cache.map.insert(s.to_string(), width);
        cache.order.push_back(s.to_string());
    });
    width
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_characters() {
        assert_eq!(char_class('a'), 0);
        assert_eq!(char_class('\u{0301}'), ZERO_WIDTH | NON_PRINTING | MARK);
        assert_eq!(char_class('\u{200D}'), ZERO_WIDTH | NON_PRINTING);
        assert_eq!(char_class('\u{0600}'), NON_PRINTING);
        assert_ne!(char_class('\u{093E}') & SPACING_MARK, 0);
        assert_eq!(char_class('\u{1734}') & SPACING_MARK, 0);
    }

    #[test]
    fn east_asian_width_matches_table() {
        assert_eq!(east_asian_width('a'), 1);
        assert_eq!(east_asian_width('\u{3000}'), 2);
        assert_eq!(east_asian_width('\u{4E00}'), 2);
        assert_eq!(east_asian_width('\u{1F600}'), 2);
        assert_eq!(east_asian_width('\u{2764}'), 1);
    }

    #[test]
    fn cache_evicts_oldest_entries() {
        for i in 0..(WIDTH_CACHE_SIZE + 10) {
            assert_eq!(visible_width(&format!("日{i}")), 2 + i.to_string().len());
        }
        WIDTH_CACHE.with(|cache| assert_eq!(cache.borrow().map.len(), WIDTH_CACHE_SIZE));
        assert_eq!(visible_width("日0"), 3);
    }
}
