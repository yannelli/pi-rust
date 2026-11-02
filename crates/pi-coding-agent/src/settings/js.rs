//! JavaScript value rules for the SDK `SettingsManager` (upstream `settings-manager.ts`): layers keep
//! `undefined` and non-finite numbers as sentinel objects, and merge, copy and stringify as JS does.

use serde_json::{Map, Value};

pub(super) use super::js_text::{ARRAY, js_number_string, normalize, parse, stringify};

/// Key of the sentinel object for `undefined`; `sdk_settings.js` uses the same keys.
pub(super) const UNDEFINED: &str = "\u{0}undefined";
/// Key of the sentinel object for `NaN`, `Infinity` and `-Infinity`.
pub(super) const NUMBER: &str = "\u{0}number";

const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

fn sentinel(key: &str, value: Value) -> Value {
    let mut map = Map::new();
    map.insert(key.to_string(), value);
    Value::Object(map)
}

fn sentinel_value<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) if map.len() == 1 => map.get(key),
        _ => None,
    }
}

pub(super) fn undefined() -> Value {
    sentinel(UNDEFINED, Value::Bool(true))
}

pub(super) fn is_undefined(value: &Value) -> bool {
    sentinel_value(value, UNDEFINED).is_some()
}

/// `None` for a missing key and for `undefined`.
pub(super) fn defined(value: Option<&Value>) -> Option<&Value> {
    value.filter(|value| !is_undefined(value))
}

/// A JavaScript number: integers stay integers, so JSON text matches `JSON.stringify`.
pub(super) fn number(value: f64) -> Value {
    if value.is_finite() && value.fract() == 0.0 && value.abs() <= MAX_SAFE_INTEGER {
        Value::from(value as i64)
    } else if value.is_finite() {
        Value::from(value)
    } else {
        sentinel(NUMBER, Value::from(js_number_string(value)))
    }
}

/// The number of a JSON number or a non-finite sentinel.
pub(super) fn as_number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        _ => match sentinel_value(value, NUMBER)?.as_str()? {
            "NaN" => Some(f64::NAN),
            "Infinity" => Some(f64::INFINITY),
            "-Infinity" => Some(f64::NEG_INFINITY),
            _ => None,
        },
    }
}

/// A plain object (`typeof "object"`, not null, not an array, not a sentinel).
pub(super) fn object(value: Option<&Value>) -> Option<&Map<String, Value>> {
    let value = value?;
    if is_undefined(value) || as_number(value).is_some() {
        return None;
    }
    value.as_object()
}

/// `typeof value === "object" && value !== null`, so arrays count.
pub(super) fn is_object_like(value: Option<&Value>) -> bool {
    matches!(value, Some(Value::Array(_))) || object(value).is_some()
}

pub(super) fn truthy(value: Option<&Value>) -> bool {
    match defined(value) {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => !text.is_empty(),
        Some(other) => as_number(other).is_none_or(|number| number != 0.0 && !number.is_nan()),
    }
}

/// `value === undefined || value === null`, the test of `??`.
pub(super) fn nullish(value: Option<&Value>) -> bool {
    matches!(defined(value), None | Some(Value::Null))
}

/// `value ?? fallback`.
pub(super) fn or(value: Option<&Value>, fallback: Value) -> Value {
    if nullish(value) { fallback } else { value.cloned().unwrap_or_else(undefined) }
}

/// `value` as returned to JavaScript: a missing key reads as `undefined`.
pub(super) fn read(value: Option<&Value>) -> Value {
    value.cloned().unwrap_or_else(undefined)
}

/// `String(value)`.
pub(super) fn js_string(value: Option<&Value>) -> String {
    let Some(value) = defined(value) else { return "undefined".into() };
    if let Some(number) = as_number(value) {
        return js_number_string(number);
    }
    match value {
        Value::Null => "null".into(),
        Value::Bool(flag) => flag.to_string(),
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| if nullish(Some(item)) { String::new() } else { js_string(Some(item)) })
            .collect::<Vec<_>>()
            .join(","),
        _ => "[object Object]".into(),
    }
}

fn string_number(text: &str) -> f64 {
    let text = text.trim();
    let radix = |prefix: &str, radix: u32| {
        text.strip_prefix(prefix)
            .or_else(|| text.strip_prefix(&prefix.to_uppercase()))
            .map(|digits| u64::from_str_radix(digits, radix).map_or(f64::NAN, |value| value as f64))
    };
    if text.is_empty() {
        return 0.0;
    }
    if let Some(value) = radix("0x", 16).or_else(|| radix("0o", 8)).or_else(|| radix("0b", 2)) {
        return value;
    }
    match text.trim_start_matches(['+', '-']) {
        "Infinity" if text.starts_with('-') => f64::NEG_INFINITY,
        "Infinity" => f64::INFINITY,
        digits if digits.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-')) => {
            text.parse().unwrap_or(f64::NAN)
        }
        _ => f64::NAN,
    }
}

/// `Number(value)`.
pub(super) fn to_number(value: Option<&Value>) -> f64 {
    let Some(value) = defined(value) else { return f64::NAN };
    if let Some(number) = as_number(value) {
        return number;
    }
    match value {
        Value::Null => 0.0,
        Value::Bool(flag) => f64::from(u8::from(*flag)),
        Value::String(text) => string_number(text),
        Value::Array(items) if items.is_empty() => 0.0,
        Value::Array(items) if items.len() == 1 => string_number(&js_string(items.first())),
        _ => f64::NAN,
    }
}

/// `{ ...value }`: objects copy, strings and arrays spread by index, other values give `{}`.
pub(super) fn spread(value: Option<&Value>) -> Map<String, Value> {
    if let Some(map) = object(value) {
        return map.clone();
    }
    match defined(value) {
        Some(Value::String(text)) => {
            text.chars().enumerate().map(|(i, c)| (i.to_string(), c.to_string().into())).collect()
        }
        Some(Value::Array(items)) => items.iter().enumerate().map(|(i, item)| (i.to_string(), item.clone())).collect(),
        _ => Map::new(),
    }
}

/// The JSON a value turns into: `undefined` is left out of objects and becomes `null` in arrays,
/// non-finite numbers become `null`.
pub(super) fn to_json(value: &Value) -> Option<Value> {
    if is_undefined(value) {
        return None;
    }
    if as_number(value).is_some_and(|number| !number.is_finite()) {
        return Some(Value::Null);
    }
    Some(match value {
        Value::Array(items) => Value::Array(items.iter().map(|item| to_json(item).unwrap_or(Value::Null)).collect()),
        Value::Object(map) => Value::Object(to_json_map(map)),
        other => other.clone(),
    })
}

/// The JSON of a settings object; the array marker of an array layer is left out.
pub(super) fn to_json_map(map: &Map<String, Value>) -> Map<String, Value> {
    map.iter()
        .filter(|(key, _)| *key != ARRAY)
        .filter_map(|(key, value)| Some((key.clone(), to_json(value)?)))
        .collect()
}

fn is_tool_modifier(value: &Value) -> bool {
    value.as_str().is_some_and(|entry| entry.starts_with('+') || entry.starts_with('-'))
}

/// `deepMergeObjects`; spreading an array layer gives a plain object, so its marker is dropped.
fn merge_objects(base: &Map<String, Value>, overrides: &Map<String, Value>) -> Map<String, Value> {
    let mut result = base.clone();
    result.shift_remove(ARRAY);
    for (key, value) in overrides {
        if is_undefined(value) || key == ARRAY {
            continue;
        }
        let merged = match (object(base.get(key)), object(Some(value))) {
            (Some(base), Some(update)) => Value::Object(merge_objects(base, update)),
            _ => value.clone(),
        };
        result.insert(key.clone(), merged);
    }
    result
}

/// Upstream `deepMergeSettings`: objects merge, other values replace, and a `defaultTools` list of
/// only `+name`/`-name` entries is appended to the inherited one.
pub(super) fn merge(base: &Map<String, Value>, overrides: &Map<String, Value>) -> Map<String, Value> {
    let mut merged = merge_objects(base, overrides);
    let (inherited, update) = (defined(base.get("defaultTools")), defined(overrides.get("defaultTools")));
    let tools = match (inherited, update) {
        (_, None) => return merged,
        (Some(Value::Array(inherited)), Some(Value::Array(update))) if update.iter().all(is_tool_modifier) => {
            Value::Array(inherited.iter().chain(update).cloned().collect())
        }
        (_, Some(update)) => update.clone(),
    };
    merged.insert("defaultTools".into(), tools);
    merged
}
