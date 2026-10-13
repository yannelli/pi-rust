//! Argument coercion toward a JSON Schema, as upstream `coerceWithJsonSchema` does: models often
//! send `"5"` for 5 or `null` for an omitted field.

use serde_json::{Map, Number, Value};

use super::check::{Checker, matches_type};

fn schema_types(schema: &Map<String, Value>) -> Vec<&str> {
    match schema.get("type") {
        Some(Value::String(kind)) => vec![kind.as_str()],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

/// JavaScript `Number(text)` for a non-blank string: decimal, exponent, or 0x/0o/0b integers.
fn parse_js_number(text: &str) -> Option<f64> {
    let text = text.trim();
    let radix = |prefix: &str, radix: u32| {
        let digits = text.strip_prefix(prefix).or_else(|| text.strip_prefix(&prefix.to_uppercase()))?;
        u64::from_str_radix(digits, radix).ok().map(|number| number as f64)
    };
    let parsed = radix("0x", 16).or_else(|| radix("0o", 8)).or_else(|| radix("0b", 2));
    let lowercase = text.to_ascii_lowercase();
    // Rust accepts `inf` and `nan` spellings that JavaScript rejects.
    let named = lowercase.contains("inf") || lowercase.contains("nan");
    parsed.or_else(|| if named { None } else { text.parse::<f64>().ok() }).filter(|number| number.is_finite())
}

fn number_value(number: f64) -> Option<Value> {
    if number.fract() == 0.0 && number.abs() < 9.007_199_254_740_992e15 {
        return Some(Value::from(number as i64));
    }
    Number::from_f64(number).map(Value::Number)
}

/// Upstream `coercePrimitiveByType`; `None` when nothing changes.
fn coerce_primitive(value: &Value, kind: &str) -> Option<Value> {
    match (kind, value) {
        ("number" | "integer", Value::Null) => Some(Value::from(0)),
        ("number" | "integer", Value::Bool(flag)) => Some(Value::from(u8::from(*flag))),
        ("number", Value::String(text)) if !text.trim().is_empty() => parse_js_number(text).and_then(number_value),
        ("integer", Value::String(text)) if !text.trim().is_empty() => {
            parse_js_number(text).filter(|number| number.fract() == 0.0).and_then(number_value)
        }
        ("boolean", Value::Null) => Some(Value::Bool(false)),
        ("boolean", Value::String(text)) if text == "true" || text == "false" => Some(Value::Bool(text == "true")),
        ("boolean", Value::Number(number)) if number.as_f64() == Some(1.0) || number.as_f64() == Some(0.0) => {
            Some(Value::Bool(number.as_f64() == Some(1.0)))
        }
        ("string", Value::Null) => Some(Value::String(String::new())),
        ("string", Value::Number(number)) => Some(Value::String(super::check::js_number(number.as_f64()?))),
        ("string", Value::Bool(flag)) => Some(Value::String(flag.to_string())),
        ("null", Value::String(text)) if text.is_empty() => Some(Value::Null),
        ("null", Value::Bool(false)) => Some(Value::Null),
        ("null", Value::Number(number)) if number.as_f64() == Some(0.0) => Some(Value::Null),
        _ => None,
    }
}

fn coerce_union(value: Value, schemas: &[Value], checker: &Checker) -> Value {
    if schemas.iter().any(|schema| checker.is_valid(&value, schema)) {
        return value;
    }
    for schema in schemas {
        let coerced = coerce(value.clone(), schema, checker);
        if checker.is_valid(&coerced, schema) {
            return coerced;
        }
    }
    value
}

/// Coerce `value` toward `schema`. Objects and arrays recurse only where the schema types them.
pub(crate) fn coerce(value: Value, schema: &Value, checker: &Checker) -> Value {
    let Some(schema) = schema.as_object() else { return value };
    let mut value = value;
    if let Some(Value::Array(schemas)) = schema.get("allOf") {
        for nested in schemas {
            value = coerce(value, nested, checker);
        }
    }
    for keyword in ["anyOf", "oneOf"] {
        if let Some(Value::Array(schemas)) = schema.get(keyword) {
            value = coerce_union(value, schemas, checker);
        }
    }
    let kinds = schema_types(schema);
    let matches_member = kinds.len() > 1 && kinds.iter().any(|kind| matches_type(&value, kind));
    if !kinds.is_empty()
        && !matches_member
        && let Some(coerced) = kinds.iter().find_map(|kind| coerce_primitive(&value, kind))
    {
        value = coerced;
    }
    match &mut value {
        Value::Object(object) if kinds.contains(&"object") => coerce_object(object, schema, checker),
        Value::Array(items) if kinds.contains(&"array") => match schema.get("items") {
            Some(Value::Array(tuple)) => {
                for (item, item_schema) in items.iter_mut().zip(tuple) {
                    *item = coerce(std::mem::take(item), item_schema, checker);
                }
            }
            Some(item_schema @ Value::Object(_)) => {
                for item in items.iter_mut() {
                    *item = coerce(std::mem::take(item), item_schema, checker);
                }
            }
            _ => {}
        },
        _ => {}
    }
    value
}

fn coerce_object(object: &mut Map<String, Value>, schema: &Map<String, Value>, checker: &Checker) {
    let properties = schema.get("properties").and_then(Value::as_object);
    for (key, property) in properties.into_iter().flatten() {
        if let Some(item) = object.get_mut(key) {
            *item = coerce(std::mem::take(item), property, checker);
        }
    }
    if let Some(additional @ Value::Object(_)) = schema.get("additionalProperties") {
        for (key, item) in object.iter_mut() {
            if !properties.is_some_and(|properties| properties.contains_key(key)) {
                *item = coerce(std::mem::take(item), additional, checker);
            }
        }
    }
}

/// Drop `null` for optional properties whose schema rejects null (upstream `normalizeOptionalNulls`).
pub(crate) fn normalize_optional_nulls(value: &mut Value, schema: &Value, checker: &Checker) {
    match value {
        Value::Array(items) => match schema.get("items") {
            Some(Value::Array(tuple)) => {
                for (item, item_schema) in items.iter_mut().zip(tuple) {
                    normalize_optional_nulls(item, item_schema, checker);
                }
            }
            Some(item_schema) => items.iter_mut().for_each(|item| normalize_optional_nulls(item, item_schema, checker)),
            None => {}
        },
        Value::Object(object) => {
            let Some(Value::Object(properties)) = schema.get("properties") else { return };
            let required: Vec<&str> = schema
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            for (key, property) in properties {
                let Some(item) = object.get_mut(key) else { continue };
                let droppable = item.is_null()
                    && !required.contains(&key.as_str())
                    && !property.get("$ref").is_some_and(Value::is_string)
                    && !checker.is_valid(&Value::Null, property);
                if droppable {
                    object.shift_remove(key);
                } else {
                    normalize_optional_nulls(item, property, checker);
                }
            }
        }
        _ => {}
    }
}
