//! Settings text as JavaScript reads and writes it: `JSON.parse` with V8's messages and number
//! rounding, `migrateSettings`, and `JSON.stringify(settings, null, 2)` with `Number#toString`.

use serde_json::{Map, Value};

use super::js::{js_string, number, to_json, to_json_map};
use super::manager::migrate;

/// Key that marks a settings layer read from a JSON array; its other keys are the indices.
pub(super) const ARRAY: &str = "\u{0}array";

const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// `Number.prototype.toString()`, from the shortest round-trip digits.
pub(super) fn js_number_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if value == 0.0 {
        return "0".into();
    }
    let sign = if value < 0.0 { "-" } else { "" };
    let exponential = format!("{:e}", value.abs());
    let (mantissa, exponent) = exponential.split_once('e').unwrap_or((&exponential, "0"));
    let digits = mantissa.replace('.', "");
    let (k, n) = (digits.len() as i64, exponent.parse::<i64>().unwrap_or(0) + 1);
    let zeros = |count: i64| "0".repeat(count.max(0) as usize);
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", zeros(n - k))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", zeros(-n))
    } else {
        let e = n - 1;
        let shown = if k == 1 { digits.clone() } else { format!("{}.{}", &digits[..1], &digits[1..]) };
        format!("{shown}e{}{}", if e >= 0 { "+" } else { "-" }, e.abs())
    };
    format!("{sign}{body}")
}

/// Numbers as JavaScript holds them: doubles, so integral floats print as integers and integers
/// beyond 2^53 round.
pub(super) fn normalize(value: Value) -> Value {
    match value {
        Value::Number(n) => match n.as_f64() {
            Some(float) if n.is_f64() || float.abs() > MAX_SAFE_INTEGER => number(float),
            _ => Value::Number(n),
        },
        Value::Array(items) => Value::Array(items.into_iter().map(normalize).collect()),
        Value::Object(map) => Value::Object(map.into_iter().map(|(key, value)| (key, normalize(value))).collect()),
        other => other,
    }
}

/// Upstream `loadFromStorage` and `persistScopedSettings`: `JSON.parse(stripBom(text))`, then
/// `migrateSettings`, which throws for a primitive and leaves an array as it is.
pub(super) fn parse(text: &str) -> Result<Map<String, Value>, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let value = serde_json::from_str::<Value>(text)
        .map_err(|error| super::json_error::message(text).unwrap_or_else(|| error.to_string()))?;
    match normalize(value) {
        Value::Object(mut map) => {
            migrate(&mut map);
            Ok(map)
        }
        Value::Array(items) => {
            let mut layer: Map<String, Value> =
                items.into_iter().enumerate().map(|(i, v)| (i.to_string(), v)).collect();
            layer.insert(ARRAY.into(), Value::Bool(true));
            Ok(layer)
        }
        other => Err(in_error(&other)),
    }
}

/// The `TypeError` of `migrateSettings` on a primitive: `"queueMode" in value`.
pub(super) fn in_error(value: &Value) -> String {
    format!("Cannot use 'in' operator to search for 'queueMode' in {}", js_string(Some(value)))
}

fn write(out: &mut String, value: &Value, indent: usize) {
    let pad = |out: &mut String, depth: usize| out.extend(std::iter::repeat_n(' ', depth));
    match value {
        Value::Number(n) if n.is_f64() => out.push_str(&js_number_string(n.as_f64().unwrap_or(f64::NAN))),
        Value::Array(items) if !items.is_empty() => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                out.push_str(if index == 0 { "\n" } else { ",\n" });
                pad(out, indent + 2);
                write(out, item, indent + 2);
            }
            out.push('\n');
            pad(out, indent);
            out.push(']');
        }
        Value::Object(map) if !map.is_empty() => {
            out.push('{');
            for (index, (key, item)) in map.iter().enumerate() {
                out.push_str(if index == 0 { "\n" } else { ",\n" });
                pad(out, indent + 2);
                out.push_str(&Value::from(key.as_str()).to_string());
                out.push_str(": ");
                write(out, item, indent + 2);
            }
            out.push('\n');
            pad(out, indent);
            out.push('}');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// `JSON.stringify(value, null, 2)` of the JSON of `value`.
pub(super) fn stringify_value(value: &Value) -> String {
    let mut out = String::new();
    write(&mut out, &to_json(value).unwrap_or(Value::Null), 0);
    out
}

/// `JSON.stringify(settings, null, 2)`.
pub(super) fn stringify(map: &Map<String, Value>) -> String {
    stringify_value(&Value::Object(to_json_map(map)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_print_as_javascript_does() {
        let cases = [(1e21, "1e+21"), (1.5e300, "1.5e+300"), (1e-7, "1e-7"), (0.000001, "0.000001"), (33.7, "33.7")];
        for (value, text) in cases {
            assert_eq!(js_number_string(value), text);
        }
        assert_eq!(js_number_string(123456789012345680000.0), "123456789012345680000");
        assert_eq!(js_number_string(-2.5), "-2.5");
        let parsed = parse(r#"{"big": 9007199254740993, "float": 2.0, "wide": 1e21}"#).unwrap();
        assert_eq!(stringify(&parsed), "{\n  \"big\": 9007199254740992,\n  \"float\": 2,\n  \"wide\": 1e+21\n}");
    }

    #[test]
    fn primitives_and_arrays_load_as_upstream_does() {
        let message = "Cannot use 'in' operator to search for 'queueMode' in";
        assert_eq!(parse("42").unwrap_err(), format!("{message} 42"));
        assert_eq!(parse("null").unwrap_err(), format!("{message} null"));
        let layer = parse("[1, \"a\"]").unwrap();
        assert_eq!(layer.get(ARRAY), Some(&Value::Bool(true)));
        assert_eq!(stringify(&layer), "{\n  \"0\": 1,\n  \"1\": \"a\"\n}");
    }
}
