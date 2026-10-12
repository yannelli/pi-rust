//! `JSON.stringify` compatible output for values that went through `JSON.parse`: numbers use
//! the ECMAScript `Number::toString` algorithm, and integer-like object keys come first in
//! ascending order, as JavaScript objects enumerate them.

use serde::Serialize;
use serde_json::Value;

/// Serialize like `JSON.stringify(value)` without indentation.
pub fn stringify(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value);
    out
}

/// [`stringify`] for any serializable value.
pub fn stringify_serialize<T: Serialize + ?Sized>(value: &T) -> String {
    serde_json::to_value(value).map(|value| stringify(&value)).unwrap_or_else(|_| "null".into())
}

fn write_value(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(number) => out.push_str(&number_to_js(number)),
        Value::String(text) => write_string(out, text),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_value(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<(&String, &Value)> = map.iter().collect();
            // Stable sort: array-index keys first (ascending), the rest keep insertion order.
            keys.sort_by_key(|(key, _)| array_index(key).map_or((1, 0), |index| (0, index)));
            out.push('{');
            for (index, (key, item)) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_string(out, key);
                out.push(':');
                write_value(out, item);
            }
            out.push('}');
        }
    }
}

fn write_string(out: &mut String, text: &str) {
    // serde_json escapes exactly the characters JSON.stringify escapes.
    out.push_str(&serde_json::to_string(text).unwrap_or_default());
}

/// A canonical array index (`0` to `2^32 - 2`), which JavaScript enumerates before other keys.
fn array_index(key: &str) -> Option<u64> {
    if key.is_empty() || !key.bytes().all(|b| b.is_ascii_digit()) || (key.len() > 1 && key.starts_with('0')) {
        return None;
    }
    key.parse::<u64>().ok().filter(|index| *index < u64::from(u32::MAX))
}

fn number_to_js(number: &serde_json::Number) -> String {
    const SAFE: u64 = 1 << 53;
    if let Some(n) = number.as_u64().filter(|n| *n <= SAFE) {
        return n.to_string();
    }
    if let Some(n) = number.as_i64().filter(|n| n.unsigned_abs() <= SAFE) {
        return n.to_string();
    }
    number.as_f64().map_or_else(|| "null".into(), |f| if f.is_finite() { js_number(f) } else { "null".into() })
}

/// ECMAScript `Number.prototype.toString()` for a finite double.
pub fn js_number(value: f64) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value == 0.0 {
        return "0".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    let sign = if value < 0.0 { "-" } else { "" };
    // Rust's `{:e}` prints the shortest round-trip digits, as ECMAScript requires.
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent: i64 = exponent.parse().unwrap_or(0);
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let k = digits.len() as i64;
    let n = exponent + 1;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        let (int, frac) = digits.split_at(n as usize);
        format!("{int}.{frac}")
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let e = n - 1;
        let e_sign = if e >= 0 { '+' } else { '-' };
        let (first, rest) = digits.split_at(1);
        let mantissa = if rest.is_empty() { first.to_string() } else { format!("{first}.{rest}") };
        format!("{mantissa}e{e_sign}{}", e.abs())
    };
    format!("{sign}{body}")
}

/// ECMAScript `Number.prototype.toFixed(digits)`: ties round away from zero on the exact value.
pub fn js_to_fixed(value: f64, digits: usize) -> String {
    if !value.is_finite() || value.abs() >= 1e21 {
        return js_number(value);
    }
    // Every finite double has a finite decimal expansion; 1100 places always cover it.
    let exact = format!("{:.1100}", value.abs());
    let (int, frac) = exact.split_once('.').unwrap_or((&exact, ""));
    let mut kept: Vec<u8> = int.bytes().chain(frac.bytes().take(digits)).collect();
    let round_up = frac.as_bytes().get(digits).is_some_and(|d| *d >= b'5');
    if round_up {
        let mut i = kept.len();
        loop {
            if i == 0 {
                kept.insert(0, b'1');
                break;
            }
            i -= 1;
            if kept[i] == b'9' {
                kept[i] = b'0';
            } else {
                kept[i] += 1;
                break;
            }
        }
    }
    let split = kept.len() - digits;
    let int = String::from_utf8_lossy(&kept[..split]).into_owned();
    let frac = String::from_utf8_lossy(&kept[split..]).into_owned();
    let sign = if value < 0.0 { "-" } else { "" };
    if digits == 0 { format!("{sign}{int}") } else { format!("{sign}{int}.{frac}") }
}
