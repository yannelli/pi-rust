//! OSC 8 hyperlink parsing and formatting.

use std::sync::LazyLock;

use regex::Regex;
use unicode_segmentation::UnicodeSegmentation;

use super::ansi::{extract_ansi_code, next_ansi_start};
use super::width::grapheme_width;

/// How an OSC sequence was terminated; links are reopened with the same terminator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Osc8Terminator {
    Bel,
    St,
}

impl Osc8Terminator {
    fn as_str(self) -> &'static str {
        match self {
            Self::Bel => "\x07",
            Self::St => "\x1b\\",
        }
    }
}

/// An open OSC 8 hyperlink.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ActiveHyperlink {
    pub(crate) params: String,
    pub(crate) url: String,
    pub(crate) terminator: Osc8Terminator,
}

/// Result of parsing one escape sequence as OSC 8.
pub(crate) enum Osc8 {
    /// Not an OSC 8 sequence.
    Other,
    /// An OSC 8 sequence with an empty URL, which closes the active link.
    Close,
    /// An OSC 8 sequence that opens a link.
    Open(ActiveHyperlink),
}

pub(crate) fn parse_osc8_hyperlink(code: &str) -> Osc8 {
    let Some(rest) = code.strip_prefix("\x1b]8;") else {
        return Osc8::Other;
    };
    let (terminator, body) = if let Some(body) = rest.strip_suffix('\x07') {
        (Osc8Terminator::Bel, body)
    } else if let Some(body) = rest.strip_suffix("\x1b\\") {
        (Osc8Terminator::St, body)
    } else {
        // Malformed: drop the two final chars like the upstream slice does.
        let cut = rest.char_indices().rev().nth(1).map_or(0, |(i, _)| i);
        (Osc8Terminator::St, &rest[..cut])
    };
    let Some((params, url)) = body.split_once(';') else {
        return Osc8::Other;
    };
    if url.is_empty() {
        return Osc8::Close;
    }
    Osc8::Open(ActiveHyperlink { params: params.to_string(), url: url.to_string(), terminator })
}

pub(crate) fn format_osc8_hyperlink(link: &ActiveHyperlink) -> String {
    format!("\x1b]8;{};{}{}", link.params, link.url, link.terminator.as_str())
}

pub(crate) fn format_osc8_close(terminator: Osc8Terminator) -> String {
    format!("\x1b]8;;{}", terminator.as_str())
}

/// The OSC 8 close sequence needed at the end of `prefix`, or an empty string when no link is open.
pub(crate) fn get_active_osc8_close(prefix: &str) -> String {
    if !prefix.contains("\x1b]8;") {
        return String::new();
    }
    let mut active: Option<ActiveHyperlink> = None;
    let mut i = 0;
    while i < prefix.len() {
        if let Some(code) = extract_ansi_code(prefix, i) {
            match parse_osc8_hyperlink(code) {
                Osc8::Other => {}
                Osc8::Close => active = None,
                Osc8::Open(link) => active = Some(link),
            }
            i += code.len();
        } else {
            i += 1;
        }
    }
    active.map_or_else(String::new, |link| format_osc8_close(link.terminator))
}

static OSC8_LINK_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new("^\x1b\\]8;[^;]*;([^\x07\x1b]*)(?:\x07|\x1b\\\\)$").unwrap_or_else(|err| panic!("osc8 regex: {err}"))
});

/// Returns the OSC 8 hyperlink URL that covers a visible terminal column.
pub fn get_osc8_link_at_column(line: &str, column: usize) -> Option<String> {
    let mut active_url: Option<String> = None;
    let mut current_col = 0;
    let mut i = 0;
    while i < line.len() {
        if let Some(code) = extract_ansi_code(line, i) {
            if let Some(caps) = OSC8_LINK_REGEX.captures(code) {
                active_url = caps.get(1).map(|m| m.as_str()).filter(|url| !url.is_empty()).map(str::to_string);
            }
            i += code.len();
            continue;
        }
        let text_end = next_ansi_start(line, i);
        for segment in line[i..text_end].graphemes(true) {
            let width = grapheme_width(segment);
            if column >= current_col && column < current_col + width {
                return active_url;
            }
            current_col += width;
        }
        i = text_end;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_open_close_and_other() {
        match parse_osc8_hyperlink("\x1b]8;id=1;https://a\x07") {
            Osc8::Open(link) => {
                assert_eq!(link.params, "id=1");
                assert_eq!(link.url, "https://a");
                assert_eq!(link.terminator, Osc8Terminator::Bel);
                assert_eq!(format_osc8_hyperlink(&link), "\x1b]8;id=1;https://a\x07");
            }
            _ => panic!("expected open"),
        }
        assert!(matches!(parse_osc8_hyperlink("\x1b]8;;\x1b\\"), Osc8::Close));
        assert!(matches!(parse_osc8_hyperlink("\x1b[31m"), Osc8::Other));
        assert!(matches!(parse_osc8_hyperlink("\x1b]8;\x07"), Osc8::Other));
    }

    #[test]
    fn finds_link_at_column_and_active_close() {
        let line = "ab\x1b]8;;https://x\x07cd\x1b]8;;\x07ef";
        assert_eq!(get_osc8_link_at_column(line, 1), None);
        assert_eq!(get_osc8_link_at_column(line, 2).as_deref(), Some("https://x"));
        assert_eq!(get_osc8_link_at_column(line, 4), None);
        assert_eq!(get_active_osc8_close("\x1b]8;;https://x\x1b\\ab"), "\x1b]8;;\x1b\\");
        assert_eq!(get_active_osc8_close(line), "");
    }
}
