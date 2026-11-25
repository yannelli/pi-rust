//! Conversions of one value: TypeBox 1.3.27's `Value.Convert` tries (`value/convert/try`) for
//! coercion pass 2, and Ajv's `coerceTypes` for pass 3. Each returns `None` when there is no result.

use serde_json::Value;

use super::explain::{js_json, strict_equal};

/// Above this, JavaScript numbers are not exact integers.
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// `result` unless it equals `value` (JavaScript `===`).
pub(super) fn changed(value: &Value, result: Option<Value>) -> Option<Value> {
    result.filter(|result| !strict_equal(result, value))
}

/// A literal taken from a value of another type that converts to exactly it.
pub(super) fn convert_literal(literal: &Value, value: &Value) -> Option<Value> {
    if strict_equal(literal, value) {
        return None;
    }
    let result = match literal {
        Value::String(_) => try_string(value),
        Value::Number(_) => try_number(value).map(number_value),
        Value::Bool(_) => try_boolean(value).map(Value::Bool),
        _ => None,
    };
    result.filter(|result| strict_equal(result, literal))
}

/// A JSON number for `number`: an integer when it is one.
pub(super) fn number_value(number: f64) -> Value {
    if number.fract() == 0.0 && number.abs() <= MAX_SAFE_INTEGER {
        return Value::from(number as i64);
    }
    serde_json::Number::from_f64(number).map_or(Value::Null, Value::Number)
}

pub(super) fn try_string(value: &Value) -> Option<Value> {
    match value {
        Value::Bool(flag) => Some(Value::String(flag.to_string())),
        Value::Number(number) => Some(Value::String(js_json(&Value::Number(number.clone())))),
        Value::Null => Some(Value::String("null".into())),
        Value::String(_) => Some(value.clone()),
        _ => None,
    }
}

pub(super) fn try_number(value: &Value) -> Option<f64> {
    let text = match value {
        Value::Bool(flag) => return Some(f64::from(u8::from(*flag))),
        Value::Number(number) => return number.as_f64(),
        Value::Null => return Some(0.0),
        Value::String(text) => text,
        _ => return None,
    };
    if let Some(number) = js_number_of_string(text) {
        return Some(number);
    }
    match text.to_lowercase().as_str() {
        "false" => Some(0.0),
        "true" => Some(1.0),
        _ => big_int_of_string(text).filter(|number| number.abs() <= MAX_SAFE_INTEGER),
    }
}

/// JavaScript `+text`: blank is 0, decimals with an exponent, or 0x/0o/0b integers; `None`
/// when that is not a finite number.
fn js_number_of_string(text: &str) -> Option<f64> {
    let text = text.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    if text.is_empty() {
        return Some(0.0);
    }
    let radix = |prefix: &str, radix: u32| {
        let digits = text.strip_prefix(prefix).or_else(|| text.strip_prefix(&prefix.to_uppercase()))?;
        u128::from_str_radix(digits, radix).ok().map(|number| number as f64)
    };
    if let Some(number) = radix("0x", 16).or_else(|| radix("0o", 8)).or_else(|| radix("0b", 2)) {
        return Some(number);
    }
    // Rust also reads `inf`, `nan` and `infinity` spellings, which are not finite or not JavaScript.
    let decimal = text.bytes().all(|byte| byte.is_ascii_digit() || b"+-.eE".contains(&byte));
    decimal.then(|| text.parse::<f64>().ok()).flatten().filter(|number| number.is_finite())
}

/// TypeBox's `TryBigInt` of a string: `12n`, `12.5` (truncated) or `12`.
fn big_int_of_string(text: &str) -> Option<f64> {
    let integer = |digits: &str| {
        let unsigned = digits.strip_prefix('-').unwrap_or(digits);
        let canonical = unsigned == "0" || (unsigned.starts_with(|c: char| ('1'..='9').contains(&c)));
        (canonical && unsigned.bytes().all(|byte| byte.is_ascii_digit())).then(|| digits.parse::<f64>().ok()).flatten()
    };
    if let Some(digits) = text.strip_suffix('n') {
        return integer(digits);
    }
    if let Some((whole, fraction)) = text.split_once('.') {
        let digits = !fraction.is_empty() && fraction.bytes().all(|byte| byte.is_ascii_digit());
        return digits.then(|| integer(whole)).flatten();
    }
    integer(text)
}

pub(super) fn try_boolean(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(flag) => Some(*flag),
        Value::Number(number) => match number.as_f64() {
            Some(0.0) => Some(false),
            Some(1.0) => Some(true),
            _ => None,
        },
        Value::Null => Some(false),
        Value::String(text) => match text.to_lowercase().as_str() {
            "false" | "0" => Some(false),
            "true" | "1" => Some(true),
            _ => None,
        },
        _ => None,
    }
}

pub(super) fn try_null(value: &Value) -> bool {
    match value {
        Value::Bool(flag) => !flag,
        Value::Number(number) => number.as_f64() == Some(0.0),
        Value::Null => true,
        Value::String(text) => matches!(text.to_lowercase().as_str(), "undefined" | "null" | "" | "0"),
        _ => false,
    }
}

/// Ajv's `coerceTypes` for one type; `None` when nothing changes.
pub(super) fn coerce_primitive(value: &Value, kind: &str) -> Option<Value> {
    match (kind, value) {
        ("number" | "integer", Value::Null) => Some(Value::from(0)),
        ("number" | "integer", Value::Bool(flag)) => Some(Value::from(u8::from(*flag))),
        ("number", Value::String(text)) if !text.trim().is_empty() => js_number_of_string(text).map(number_value),
        ("integer", Value::String(text)) if !text.trim().is_empty() => {
            js_number_of_string(text).filter(|number| number.fract() == 0.0).map(number_value)
        }
        ("boolean", Value::Null) => Some(Value::Bool(false)),
        ("boolean", Value::String(text)) if text == "true" || text == "false" => Some(Value::Bool(text == "true")),
        ("boolean", Value::Number(number)) => match number.as_f64() {
            Some(1.0) => Some(Value::Bool(true)),
            Some(0.0) => Some(Value::Bool(false)),
            _ => None,
        },
        ("string", Value::Null) => Some(Value::String(String::new())),
        ("string", Value::Number(_) | Value::Bool(_)) => try_string(value),
        ("null", Value::String(text)) if text.is_empty() => Some(Value::Null),
        ("null", Value::Bool(false)) => Some(Value::Null),
        ("null", Value::Number(number)) if number.as_f64() == Some(0.0) => Some(Value::Null),
        _ => None,
    }
}
