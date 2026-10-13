//! RGI emoji recognition, the Rust replacement for `/^\p{RGI_Emoji}$/v`.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::LazyLock;

use super::emoji_table::{
    BASIC_RANGES, FLAG_REGIONS, KEYCAP_BASES, MODIFIER_BASE_RANGES, TAG_SEQUENCES, VS16_RANGES, ZWJ_SEQUENCES,
};
use super::width::in_ranges;

const ZWJ: char = '\u{200D}';
const VS16: char = '\u{FE0F}';

/// Skeleton (tones removed) to tone mode, parsed from the packed table.
static ZWJ_TABLE: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    ZWJ_SEQUENCES.split_ascii_whitespace().map(|entry| entry.split_once(':').unwrap_or((entry, ""))).collect()
});

fn is_tone(c: char) -> bool {
    ('\u{1F3FB}'..='\u{1F3FF}').contains(&c)
}

fn is_regional_indicator(c: char) -> bool {
    ('\u{1F1E6}'..='\u{1F1FF}').contains(&c)
}

/// Cheap pre-check before the RGI lookup: emoji blocks, VS16, or more than two UTF-16 code units.
pub(crate) fn could_be_emoji(segment: &str) -> bool {
    let Some(first) = segment.chars().next() else {
        return false;
    };
    let cp = first as u32;
    (0x1F000..=0x1FBFF).contains(&cp)
        || (0x2300..=0x23FF).contains(&cp)
        || (0x2600..=0x27BF).contains(&cp)
        || (0x2B50..=0x2B55).contains(&cp)
        || segment.contains(VS16)
        || segment.chars().map(char::len_utf16).sum::<usize>() > 2
}

/// Returns true when the whole string is exactly one RGI emoji (Unicode `RGI_Emoji`, Emoji 17.0).
pub fn is_rgi_emoji(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    match chars.as_slice() {
        [] => false,
        [c] => in_ranges(BASIC_RANGES, *c as u32),
        [c, VS16] => in_ranges(VS16_RANGES, *c as u32),
        [c, tone] if is_tone(*tone) => in_ranges(MODIFIER_BASE_RANGES, *c as u32),
        [a, b] if is_regional_indicator(*a) && is_regional_indicator(*b) => is_flag(*a, *b),
        [key, VS16, '\u{20E3}'] => KEYCAP_BASES.contains(*key),
        ['\u{1F3F4}', .., '\u{E007F}'] => {
            TAG_SEQUENCES.iter().any(|seq| seq.iter().copied().eq(chars.iter().map(|&c| c as u32)))
        }
        _ if chars.contains(&ZWJ) => is_zwj_sequence(&chars),
        _ => false,
    }
}

fn is_flag(a: char, b: char) -> bool {
    let letter = |c: char| b'A' + (c as u32 - 0x1F1E6) as u8;
    let pair = [letter(a), letter(b)];
    FLAG_REGIONS.split_ascii_whitespace().any(|region| region.as_bytes() == pair)
}

fn is_zwj_sequence(chars: &[char]) -> bool {
    let mut key = String::with_capacity(chars.len() * 6);
    let mut tones: Vec<Option<u32>> = Vec::new();
    for (index, element) in chars.split(|&c| c == ZWJ).enumerate() {
        if index > 0 {
            key.push('-');
        }
        let [base, rest @ ..] = element else {
            return false;
        };
        let _ = write!(key, "{:X}", *base as u32);
        match rest {
            [] => tones.push(None),
            [VS16] => {
                key.push('!');
                tones.push(None);
            }
            [tone] if is_tone(*tone) => tones.push(Some(*tone as u32 - 0x1F3FB)),
            _ => return false,
        }
    }
    ZWJ_TABLE.get(key.as_str()).is_some_and(|mode| tone_mode_accepts(mode, &tones))
}

/// Checks the tones of a sequence against a packed tone mode (see `emoji_table.rs`).
fn tone_mode_accepts(mode: &str, tones: &[Option<u32>]) -> bool {
    if tones.iter().all(Option::is_none) {
        return !mode.contains('x');
    }
    let slots: Vec<usize> = mode.chars().filter_map(|c| c.to_digit(10)).map(|d| d as usize).collect();
    if tones.iter().enumerate().any(|(i, tone)| tone.is_some() != slots.contains(&i)) {
        return false;
    }
    if mode.contains('d') {
        let used: Vec<u32> = tones.iter().flatten().copied().collect();
        return used.iter().enumerate().all(|(i, tone)| !used[i + 1..].contains(tone));
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_each_sequence_type() {
        for emoji in [
            "\u{1F600}",
            "\u{2764}\u{FE0F}",
            "\u{261D}\u{1F3FB}",
            "\u{1F1EF}\u{1F1F5}",
            "#\u{FE0F}\u{20E3}",
            "\u{1F3F4}\u{E0067}\u{E0062}\u{E0073}\u{E0063}\u{E0074}\u{E007F}",
            "\u{1F468}\u{200D}\u{1F4BB}",
            "\u{1F469}\u{1F3FD}\u{200D}\u{1F4BB}",
            "\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}",
            "\u{1F9D1}\u{1F3FB}\u{200D}\u{1F91D}\u{200D}\u{1F9D1}\u{1F3FB}",
            "\u{1FAF1}\u{1F3FB}\u{200D}\u{1FAF2}\u{1F3FC}",
        ] {
            assert!(is_rgi_emoji(emoji), "{emoji:?}");
        }
    }

    #[test]
    fn rejects_non_rgi_sequences() {
        for text in [
            "",
            "a",
            "\u{2764}",
            "\u{1F1EF}\u{1F1EF}",
            "1\u{20E3}",
            "\u{1F468}\u{200D}\u{1F9B0}\u{200D}\u{1F4BB}",
            "\u{1F3F3}\u{200D}\u{1F308}",
            "\u{1FAF1}\u{1F3FB}\u{200D}\u{1FAF2}\u{1F3FB}",
            "\u{1FAF1}\u{200D}\u{1FAF2}",
            "\u{1F468}\u{200D}",
        ] {
            assert!(!is_rgi_emoji(text), "{text:?}");
        }
    }
}
