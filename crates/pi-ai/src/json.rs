//! Best-effort parsing of incomplete JSON from streamed tool arguments.

use serde_json::Value;

/// Parse `input`, closing open strings, arrays, and objects. Trailing tokens
/// that cannot complete (a key without a value, a partial literal) are dropped.
pub fn parse_streaming_json(input: &str) -> Option<Value> {
    if input.trim().is_empty() {
        return None;
    }
    if let Ok(value) = serde_json::from_str(input) {
        return Some(value);
    }
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
}
