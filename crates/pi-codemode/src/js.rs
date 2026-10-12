//! Helpers that reproduce JavaScript string and number formatting, so text matches upstream output.

use serde_json::Value;

/// `JSON.stringify(value)` for a JSON value: compact, with JavaScript number formatting.
pub fn js_stringify(value: &Value) -> String {
    let mut out = String::new();
    write_json(value, &mut out);
    out
}

fn write_json(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => out.push_str(&js_number_json(number)),
        Value::String(text) => out.push_str(&quote(text)),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_json(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (index, (key, item)) in map.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&quote(key));
                out.push(':');
                write_json(item, out);
            }
            out.push('}');
        }
    }
}

/// A JSON string literal, escaped like `JSON.stringify`.
pub fn quote(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into())
}

fn js_number_json(number: &serde_json::Number) -> String {
    if let Some(int) = number.as_i64() {
        return int.to_string();
    }
    if let Some(int) = number.as_u64() {
        return int.to_string();
    }
    number.as_f64().map(js_number).unwrap_or_else(|| number.to_string())
}

/// Parse JSON text from `JSON.stringify`, which escapes lone surrogates as `\\udXXX`. Those
/// become U+FFFD, since Rust strings hold only Unicode scalar values.
pub fn parse_js_json(text: &str) -> serde_json::Result<Value> {
    if !text.contains("\\u") {
        return serde_json::from_str(text);
    }
    serde_json::from_str(&replace_lone_surrogates(text))
}

fn surrogate_at(bytes: &[u8], index: usize) -> Option<u16> {
    if bytes.get(index) != Some(&b'\\') || bytes.get(index + 1) != Some(&b'u') {
        return None;
    }
    let hex = std::str::from_utf8(bytes.get(index + 2..index + 6)?).ok()?;
    u16::from_str_radix(hex, 16).ok().filter(|unit| (0xd800..0xe000).contains(unit))
}

fn replace_lone_surrogates(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut start = 0;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\\' {
            index += 1;
            continue;
        }
        match surrogate_at(bytes, index) {
            Some(0xd800..0xdc00) if surrogate_at(bytes, index + 6).is_some_and(|low| low >= 0xdc00) => index += 12,
            Some(_) => {
                out.push_str(&text[start..index]);
                out.push_str("\\ufffd");
                index += 6;
                start = index;
            }
            None => index += 2,
        }
    }
    out.push_str(&text[start..]);
    out
}

/// `String(number)` for a finite number (`Number.prototype.toString` with radix 10).
pub fn js_number(value: f64) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if value == 0.0 {
        return "0".into();
    }
    if value < 0.0 {
        return format!("-{}", js_number(-value));
    }
    // Shortest round-trip digits, then the layout rules of Number::toString.
    let exp_form = format!("{value:e}");
    let (mantissa, exponent) = exp_form.split_once('e').unwrap_or((&exp_form, "0"));
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let exponent: i64 = exponent.parse().unwrap_or(0);
    let k = digits.len() as i64;
    let n = exponent + 1;
    if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let sign = if n - 1 < 0 { '-' } else { '+' };
        let power = (n - 1).abs();
        if k == 1 {
            format!("{digits}e{sign}{power}")
        } else {
            format!("{}.{}e{sign}{power}", &digits[..1], &digits[1..])
        }
    }
}

/// `Number.prototype.toFixed(digits)` for small non-negative values: ties round up like JavaScript.
pub fn js_to_fixed(value: f64, digits: usize) -> String {
    let scale = 10f64.powi(digits as i32);
    let scaled = value * scale;
    if scaled.fract() == 0.5 {
        return format!("{:.*}", digits, (scaled.floor() + 1.0) / scale);
    }
    format!("{value:.digits$}")
}

/// `String.prototype.trim()`: Unicode white space plus the byte order mark.
pub fn js_trim(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// Length in UTF-16 code units, like `string.length`.
pub fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// Byte offset of UTF-16 index `index`, rounded down to a character boundary.
fn byte_offset(text: &str, index: usize) -> usize {
    let mut units = 0;
    for (offset, c) in text.char_indices() {
        if units + c.len_utf16() > index {
            return offset;
        }
        units += c.len_utf16();
    }
    text.len()
}

/// `text.slice(start, end)` by UTF-16 index, never splitting a character.
pub fn utf16_slice(text: &str, start: usize, end: usize) -> &str {
    let from = byte_offset(text, start);
    let to = byte_offset(text, end).max(from);
    &text[from..to]
}

/// The last `count` UTF-16 code units of `text`, like `text.slice(-count)`.
pub fn utf16_tail(text: &str, count: usize) -> &str {
    let len = utf16_len(text);
    utf16_slice(text, len.saturating_sub(count), len)
}

/// `a.localeCompare(b)` approximated for identifiers: punctuation before digits before letters,
/// letters case-insensitively first, then lowercase before uppercase.
pub fn locale_compare(a: &str, b: &str) -> std::cmp::Ordering {
    fn class(c: char) -> u8 {
        if c.is_whitespace() {
            0
        } else if c.is_ascii_punctuation() {
            1
        } else if c.is_numeric() {
            3
        } else if c.is_alphabetic() {
            4
        } else {
            2
        }
    }
    let primary = |s: &str| s.chars().map(|c| (class(c), c.to_lowercase().next().unwrap_or(c))).collect::<Vec<_>>();
    let tertiary = |s: &str| s.chars().map(|c| c.is_uppercase()).collect::<Vec<_>>();
    primary(a).cmp(&primary(b)).then_with(|| tertiary(a).cmp(&tertiary(b))).then_with(|| a.cmp(b))
}

/// Compare by UTF-16 code units, like the default `Array.prototype.sort`.
pub fn utf16_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn formats_numbers_like_javascript() {
        assert_eq!(js_number(1.0), "1");
        assert_eq!(js_number(1.5), "1.5");
        assert_eq!(js_number(1e21), "1e+21");
        assert_eq!(js_number(1e20), "100000000000000000000");
        assert_eq!(js_number(1e-7), "1e-7");
        assert_eq!(js_number(0.000001), "0.000001");
        assert_eq!(js_number(-2.5e-10), "-2.5e-10");
        assert_eq!(
            js_stringify(&json!({"a": [1, 2.5, null, "x\n"], "b": true})),
            r#"{"a":[1,2.5,null,"x\n"],"b":true}"#
        );
    }

    #[test]
    fn replaces_lone_surrogate_escapes() {
        assert_eq!(parse_js_json(r#""a\ud800b""#).unwrap(), json!("a\u{fffd}b"));
        assert_eq!(parse_js_json(r#""\ud83d\ude00""#).unwrap(), json!("😀"));
        assert_eq!(parse_js_json(r#""\\ud800""#).unwrap(), json!("\\ud800"));
        assert_eq!(parse_js_json(r#"{"\udc00":1}"#).unwrap(), json!({"\u{fffd}": 1}));
    }

    #[test]
    fn rounds_ties_up_in_to_fixed() {
        assert_eq!(js_to_fixed(0.25, 1), "0.3");
        assert_eq!(js_to_fixed(1.25, 1), "1.3");
        assert_eq!(js_to_fixed(0.04, 1), "0.0");
        assert_eq!(js_to_fixed(12.345, 1), "12.3");
    }

    #[test]
    fn slices_by_utf16_units() {
        assert_eq!(utf16_len("a😀b"), 4);
        assert_eq!(utf16_slice("a😀b", 0, 2), "a");
        assert_eq!(utf16_slice("a😀b", 0, 3), "a😀");
        assert_eq!(utf16_tail("abcdef", 2), "ef");
    }

    #[test]
    fn compares_like_locale_compare() {
        use std::cmp::Ordering::*;
        assert_eq!(locale_compare("mcp__docs", "mcp__github"), Less);
        assert_eq!(locale_compare("a_b", "ab"), Less);
        assert_eq!(locale_compare("B", "a"), Greater);
        assert_eq!(locale_compare("a", "A"), Less);
    }
}
