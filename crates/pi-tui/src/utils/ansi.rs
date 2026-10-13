//! Detection and removal of ANSI CSI, OSC, and APC escape sequences.

/// Length in bytes of the CSI/OSC/APC sequence starting at byte `pos`, or 0 if there is none.
pub(crate) fn ansi_code_length(s: &str, pos: usize) -> usize {
    let bytes = s.as_bytes();
    if pos >= bytes.len() || bytes[pos] != 0x1b {
        return 0;
    }
    match bytes.get(pos + 1) {
        // CSI: ESC [ ... ended by m, G, K, H, or J.
        Some(b'[') => bytes[pos + 2..]
            .iter()
            .position(|b| matches!(b, b'm' | b'G' | b'K' | b'H' | b'J'))
            .map_or(0, |offset| offset + 3),
        // OSC (ESC ]) and APC (ESC _), ended by BEL or ST (ESC \).
        Some(b']' | b'_') => {
            for j in pos + 2..bytes.len() {
                if bytes[j] == 0x07 {
                    return j + 1 - pos;
                }
                if bytes[j] == 0x1b && bytes.get(j + 1) == Some(&b'\\') {
                    return j + 2 - pos;
                }
            }
            0
        }
        _ => 0,
    }
}

/// Returns the escape sequence that starts at byte `pos`, if any. Its `len()` is the sequence length.
pub fn extract_ansi_code(s: &str, pos: usize) -> Option<&str> {
    let length = ansi_code_length(s, pos);
    (length > 0).then(|| &s[pos..pos + length])
}

/// Byte offset of the next escape sequence at or after `from`, or `s.len()` when there is none.
pub(crate) fn next_ansi_start(s: &str, from: usize) -> usize {
    let bytes = s.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        match bytes[i..].iter().position(|&b| b == 0x1b) {
            None => return bytes.len(),
            Some(offset) => {
                i += offset;
                if ansi_code_length(s, i) > 0 {
                    return i;
                }
                i += 1;
            }
        }
    }
    bytes.len()
}

/// Byte offset of the next tab or escape sequence at or after `from`, or `s.len()`.
pub(crate) fn next_tab_or_ansi(s: &str, from: usize) -> usize {
    let bytes = s.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        if bytes[i] == b'\t' || ansi_code_length(s, i) > 0 {
            return i;
        }
        i += 1;
    }
    bytes.len()
}

/// Length in bytes of the char that starts at byte `pos` (which must be a char boundary).
pub(crate) fn char_len_at(s: &str, pos: usize) -> usize {
    s[pos..].chars().next().map_or(1, char::len_utf8)
}

/// Removes ANSI, OSC, and APC control sequences while keeping visible text.
pub fn strip_terminal_sequences(s: &str) -> String {
    if !s.contains('\x1b') {
        return s.to_string();
    }
    let mut result = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let length = ansi_code_length(s, i);
        if length > 0 {
            i += length;
            continue;
        }
        let next = next_ansi_start(s, i + char_len_at(s, i));
        result.push_str(&s[i..next]);
        i = next;
    }
    result
}

/// Prepares text for terminal output: Thai/Lao AM vowels become their same-width decompositions (fewer
/// stale cells on repaint) and visible tabs become three spaces. Tabs inside escape sequences stay as is.
pub fn normalize_terminal_output(s: &str) -> String {
    let normalized = if s.contains(['\u{0E33}', '\u{0EB3}']) {
        s.replace('\u{0E33}', "\u{0E4D}\u{0E32}").replace('\u{0EB3}', "\u{0ECD}\u{0EB2}")
    } else {
        s.to_string()
    };
    if !normalized.contains('\t') {
        return normalized;
    }
    let mut result = String::with_capacity(normalized.len() + 8);
    let mut i = 0;
    while i < normalized.len() {
        let length = ansi_code_length(&normalized, i);
        if length > 0 {
            result.push_str(&normalized[i..i + length]);
            i += length;
            continue;
        }
        let len = char_len_at(&normalized, i);
        let piece = &normalized[i..i + len];
        result.push_str(if piece == "\t" { "   " } else { piece });
        i += len;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_csi_osc_and_apc_sequences() {
        assert_eq!(ansi_code_length("\x1b[31mx", 0), 5);
        assert_eq!(ansi_code_length("\x1b]8;;u\x07x", 0), 7);
        assert_eq!(ansi_code_length("\x1b_pi\x1b\\x", 0), 6);
        assert_eq!(ansi_code_length("\x1b[31", 0), 0);
        assert_eq!(ansi_code_length("\x1bx", 0), 0);
        assert_eq!(extract_ansi_code("ab\x1b[0mc", 2), Some("\x1b[0m"));
        assert_eq!(extract_ansi_code("ab\x1b[0mc", 1), None);
    }

    #[test]
    fn strips_sequences_and_keeps_lone_escapes() {
        assert_eq!(strip_terminal_sequences("\x1b[1mé\x1b[0m\x1bz"), "é\x1bz");
        assert_eq!(next_ansi_start("a\x1bb\x1b[0m", 0), 3);
    }
}
