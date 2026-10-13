//! JSON repair for provider output and best-effort parsing of streamed tool arguments,
//! ported from upstream `utils/json-parse.ts` and `utils/sanitize-unicode.ts`.

use serde_json::Value;

const VALID_ESCAPES: &[char] = &['"', '\\', '/', 'b', 'f', 'n', 'r', 't', 'u'];

fn surrogate_escape(chars: &[char], index: usize) -> Option<u16> {
    if chars.get(index) != Some(&'\\') || chars.get(index + 1) != Some(&'u') {
        return None;
    }
    let digits: String = chars.get(index + 2..index + 6)?.iter().collect();
    u16::from_str_radix(&digits, 16).ok().filter(|code| (0xD800..=0xDFFF).contains(code))
}

/// Repair string literals: escape raw control characters, double backslashes before invalid
/// escapes, and drop unpaired surrogate escapes (Rust strings cannot hold them).
pub fn repair_json(json: &str) -> String {
    repair(json, false)
}

/// `streaming` drops a trailing lone backslash: the rest of its escape has not arrived yet.
fn repair(json: &str, streaming: bool) -> String {
    let chars: Vec<char> = json.chars().collect();
    let mut repaired = String::with_capacity(json.len());
    let mut in_string = false;
    let mut index = 0;
    while index < chars.len() {
        let character = chars[index];
        index += 1;
        if !in_string || character == '"' {
            in_string ^= character == '"';
            repaired.push(character);
            continue;
        }
        if character != '\\' {
            match character {
                '\u{8}' => repaired.push_str("\\b"),
                '\u{c}' => repaired.push_str("\\f"),
                '\n' => repaired.push_str("\\n"),
                '\r' => repaired.push_str("\\r"),
                '\t' => repaired.push_str("\\t"),
                control if (control as u32) < 0x20 => repaired.push_str(&format!("\\u{:04x}", control as u32)),
                other => repaired.push(other),
            }
            continue;
        }
        let start = index - 1;
        if let Some(code) = surrogate_escape(&chars, start) {
            let low = surrogate_escape(&chars, start + 6).filter(|low| (0xDC00..=0xDFFF).contains(low));
            if (0xD800..=0xDBFF).contains(&code) && low.is_some() {
                repaired.extend(&chars[start..start + 12]);
                index = start + 12;
            } else {
                index = start + 6;
            }
            continue;
        }
        match chars.get(index) {
            Some('u') if chars.get(index + 1..index + 5).is_some_and(|d| d.iter().all(char::is_ascii_hexdigit)) => {
                repaired.extend(&chars[start..start + 6]);
                index = start + 6;
            }
            Some(next) if VALID_ESCAPES.contains(next) => {
                repaired.push('\\');
                repaired.push(*next);
                index += 1;
            }
            None if streaming => {}
            _ => repaired.push_str("\\\\"),
        }
    }
    repaired
}

/// Parse `json`, falling back to [`repair_json`] when the text is malformed.
pub fn parse_json_with_repair(json: &str) -> Result<Value, serde_json::Error> {
    serde_json::from_str(json).or_else(|error| {
        let repaired = repair_json(json);
        if repaired == json { Err(error) } else { serde_json::from_str(&repaired) }
    })
}

/// Parse `input`, closing open strings, arrays, and objects. Trailing tokens
/// that cannot complete (a key without a value, a partial literal) are dropped.
pub fn parse_streaming_json(input: &str) -> Option<Value> {
    if input.trim().is_empty() {
        return None;
    }
    if let Ok(value) = parse_json_with_repair(input) {
        return Some(value);
    }
    partial_parse(&repair(input, true))
}

fn partial_parse(input: &str) -> Option<Value> {
    let mut end = input.len();
    for _ in 0..64 {
        let candidate = &input[..end];
        if let Some(value) = close_and_parse(candidate) {
            return Some(value);
        }
        end = candidate.char_indices().next_back()?.0;
        if end == 0 {
            return None;
        }
    }
    None
}

fn close_and_parse(prefix: &str) -> Option<Value> {
    let mut stack = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    for character in prefix.chars() {
        if in_string {
            match character {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match character {
            '"' => in_string = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                stack.pop();
            }
            _ => {}
        }
    }
    if escaped {
        return None;
    }
    let mut repaired = prefix.trim_end().to_string();
    if in_string {
        repaired.push('"');
    }
    while repaired.ends_with(',') {
        repaired.pop();
    }
    repaired.extend(stack.iter().rev());
    serde_json::from_str(&repaired).ok()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn repairs_partial_objects() {
        assert_eq!(parse_streaming_json(r#"{"path":"a.t"#), Some(json!({"path": "a.t"})));
        assert_eq!(parse_streaming_json(r#"{"path":"a","lim"#), Some(json!({"path": "a"})));
        assert_eq!(parse_streaming_json(r#"{"edits":[{"oldText":"x","#), Some(json!({"edits": [{"oldText": "x"}]})));
        assert_eq!(parse_streaming_json(r#"{"n":12"#), Some(json!({"n": 12})));
        assert_eq!(parse_streaming_json(r#"{"s":"a\"#), Some(json!({"s": "a"})));
        assert_eq!(parse_streaming_json(""), None);
    }

    #[test]
    fn repairs_control_characters_and_invalid_escapes() {
        let raw = "{\"path\":\"C:\\Users\\x\",\"text\":\"a\tb\nc\u{1}\"}";
        assert_eq!(repair_json(raw), r#"{"path":"C:\\Users\\x","text":"a\tb\nc\u0001"}"#);
        let value = parse_json_with_repair(raw).unwrap();
        assert_eq!(value, json!({"path": "C:\\Users\\x", "text": "a\tb\nc\u{1}"}));
        assert_eq!(parse_json_with_repair(r#"{"a":"\u00e9\/"}"#).unwrap(), json!({"a": "\u{e9}/"}));
        assert!(parse_json_with_repair("{").is_err());
        assert_eq!(parse_streaming_json("{\"a\":\"x\\q"), Some(json!({"a": "x\\q"})));
    }

    #[test]
    fn drops_unpaired_surrogate_escapes() {
        let value = parse_json_with_repair(r#"{"t":"a\ud83d b\ude48 c\ud83d\ude48"}"#).unwrap();
        assert_eq!(value, json!({"t": "a b c\u{1f648}"}));
    }
}
