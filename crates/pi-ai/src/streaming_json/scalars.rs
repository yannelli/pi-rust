//! Tokens outside strings: delimiters, escapes, numbers and literals.

use serde_json::Value;

const LITERALS: [(&str, Value); 3] =
    [("true", Value::Bool(true)), ("false", Value::Bool(false)), ("null", Value::Null)];

pub(super) fn simple_escape(byte: u8) -> Option<&'static str> {
    Some(match byte {
        b'"' => "\"",
        b'\\' => "\\",
        b'/' => "/",
        b'b' => "\u{8}",
        b'f' => "\u{c}",
        b'n' => "\n",
        b'r' => "\r",
        b't' => "\t",
        _ => return None,
    })
}

pub(super) fn is_delimiter(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | b',' | b':' | b'{' | b'}' | b'[' | b']' | b'"')
}

/// The value of a complete number or literal token; `None` for anything else, which is dropped.
pub(super) fn scalar_value(token: &str) -> Option<Value> {
    if let Some((_, value)) = LITERALS.iter().find(|(literal, _)| *literal == token) {
        return Some(value.clone());
    }
    match number_shape(token) {
        NumberShape::Complete => serde_json::from_str(token).ok(),
        _ => None,
    }
}

/// The value an unterminated number or literal token shows.
pub(super) fn pending_scalar_value(token: &str) -> Option<Value> {
    if let Some((_, value)) = LITERALS.iter().find(|(literal, _)| literal.starts_with(token)) {
        return Some(value.clone());
    }
    match number_shape(token) {
        NumberShape::Complete => serde_json::from_str(token).ok(),
        NumberShape::MissingExponentDigits(mantissa) => serde_json::from_str(&token[..mantissa]).ok(),
        NumberShape::Invalid => None,
    }
}

enum NumberShape {
    Complete,
    /// A mantissa of this length followed by `e` or `E` and an optional sign.
    MissingExponentDigits(usize),
    Invalid,
}

/// How `token` matches the JSON number grammar.
fn number_shape(token: &str) -> NumberShape {
    let bytes = token.as_bytes();
    let digits = |from: usize| bytes[from..].iter().take_while(|byte| byte.is_ascii_digit()).count();
    let mut end = usize::from(bytes.first() == Some(&b'-'));
    match bytes.get(end) {
        Some(b'0') => end += 1,
        Some(b'1'..=b'9') => end += digits(end),
        _ => return NumberShape::Invalid,
    }
    if bytes.get(end) == Some(&b'.') {
        let fraction = digits(end + 1);
        if fraction == 0 {
            return NumberShape::Invalid;
        }
        end += 1 + fraction;
    }
    let mantissa = end;
    if end == bytes.len() {
        return NumberShape::Complete;
    }
    if !matches!(bytes[end], b'e' | b'E') {
        return NumberShape::Invalid;
    }
    end += 1;
    if matches!(bytes.get(end), Some(b'+' | b'-')) {
        end += 1;
    }
    match digits(end) {
        0 if end == bytes.len() => NumberShape::MissingExponentDigits(mantissa),
        count if count > 0 && end + count == bytes.len() => NumberShape::Complete,
        _ => NumberShape::Invalid,
    }
}
