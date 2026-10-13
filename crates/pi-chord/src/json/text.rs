//! UTF-16 measurements (JavaScript string lengths) and `JSON.stringify` output.

use std::fmt::{self, Write};

use super::Json;

/// The JavaScript `length` of a string: its UTF-16 code unit count.
pub(crate) fn utf16_len(text: &str) -> usize {
    if text.is_ascii() { text.len() } else { text.chars().map(char::len_utf16).sum() }
}

/// The byte offset after `units` UTF-16 code units, clamped to the end like `String.slice`.
/// `None` when the offset would split a surrogate pair, which Rust strings cannot hold.
pub(crate) fn utf16_offset(text: &str, units: usize) -> Option<usize> {
    if text.is_ascii() {
        return Some(units.min(text.len()));
    }
    let mut seen = 0;
    for (offset, character) in text.char_indices() {
        if seen == units {
            return Some(offset);
        }
        seen += character.len_utf16();
        if seen > units {
            return None;
        }
    }
    Some(text.len())
}

pub(crate) fn write_json_string(out: &mut impl Write, text: &str) -> fmt::Result {
    out.write_char('"')?;
    let mut start = 0;
    for (index, byte) in text.bytes().enumerate() {
        let escape = match byte {
            b'"' => "\\\"",
            b'\\' => "\\\\",
            b'\n' => "\\n",
            b'\r' => "\\r",
            b'\t' => "\\t",
            0x08 => "\\b",
            0x0c => "\\f",
            0..=0x1f => "",
            _ => continue,
        };
        out.write_str(&text[start..index])?;
        if escape.is_empty() {
            write!(out, "\\u{byte:04x}")?;
        } else {
            out.write_str(escape)?;
        }
        start = index + 1;
    }
    out.write_str(&text[start..])?;
    out.write_char('"')
}

fn write_json(out: &mut impl Write, value: &Json) -> fmt::Result {
    match value {
        Json::Null => out.write_str("null"),
        Json::Bool(value) => out.write_str(if *value { "true" } else { "false" }),
        Json::Number(value) => write!(out, "{value}"),
        Json::String(value) => write_json_string(out, value),
        Json::Array(items) => {
            out.write_char('[')?;
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.write_char(',')?;
                }
                write_json(out, item)?;
            }
            out.write_char(']')
        }
        Json::Object(map) => {
            out.write_char('{')?;
            for (index, (key, item)) in map.iter().enumerate() {
                if index > 0 {
                    out.write_char(',')?;
                }
                write_json_string(out, key)?;
                out.write_char(':')?;
                write_json(out, item)?;
            }
            out.write_char('}')
        }
    }
}

impl fmt::Display for Json {
    /// Compact JSON text byte-identical to JavaScript `JSON.stringify`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_json(f, self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_utf16() {
        assert_eq!(utf16_len("a\u{1F600}b"), 4);
        assert_eq!(utf16_offset("a\u{1F600}b", 1), Some(1));
        assert_eq!(utf16_offset("a\u{1F600}b", 2), None);
        assert_eq!(utf16_offset("a\u{1F600}b", 3), Some(5));
        assert_eq!(utf16_offset("abc", 9), Some(3));
    }

    #[test]
    fn stringifies_like_javascript() {
        let value = crate::json!({"a": "x\"\u{1}\n/é", "b": [1, 2.5, null, true], "1": {}});
        assert_eq!(value.to_string(), r#"{"1":{},"a":"x\"\u0001\n/é","b":[1,2.5,null,true]}"#);
    }
}
