//! Port of upstream `schema.ts`: canonical JSON, the 64-bit fingerprint hash, input normalization and
//! identity, validation with TypeBox's messages, and the structural `fits` check of contracts.

mod fits;

use std::borrow::Cow;
use std::cmp::Ordering;
use std::fmt::Write;

pub use fits::fits;
use pi_schema::SchemaError;

use super::errors::invalid;
use crate::error::ChordResult;
use crate::json::{Json, JsonMap, write_json_string};

/// JavaScript's default sort order of strings: by UTF-16 code units.
pub(crate) fn utf16_cmp(left: &str, right: &str) -> Ordering {
    if left.is_ascii() && right.is_ascii() {
        return left.cmp(right);
    }
    left.encode_utf16().cmp(right.encode_utf16())
}

/// Canonical JSON: object keys sorted by UTF-16 code units, the rest as `JSON.stringify` writes it.
pub fn canonical_json(value: &Json) -> String {
    let mut out = String::new();
    write_canonical(&mut out, value);
    out
}

fn write_canonical(out: &mut String, value: &Json) {
    match value {
        Json::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(out, item);
            }
            out.push(']');
        }
        Json::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|(left, _), (right, _)| utf16_cmp(left, right));
            out.push('{');
            for (index, (key, item)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                let _ = write_json_string(out, key);
                out.push(':');
                write_canonical(out, item);
            }
            out.push('}');
        }
        _ => {
            let _ = write!(out, "{value}");
        }
    }
}

/// Non-cryptographic 64-bit hash (two FNV-1a lanes over UTF-16 code units, `Math.imul` arithmetic).
/// Contracts are trusted, so collisions are not an attack surface.
pub fn hash64(text: &str) -> String {
    let mut a: u32 = 0x811c_9dc5;
    let mut b: u32 = 0x0100_0193 ^ 0x5bd1_e995;
    for code in text.encode_utf16() {
        let code = u32::from(code);
        a = (a ^ code).wrapping_mul(0x0100_0193);
        b = (b ^ code).wrapping_mul(0x5bd1_e995) ^ (b >> 13);
    }
    format!("{a:08x}{b:08x}")
}

/// Deep equality as canonical JSON compares it: object key order does not matter.
pub fn deep_equal(left: &Json, right: &Json) -> bool {
    left == right
}

/// [`deep_equal`] where a missing value (`undefined`) equals `null`, as canonical JSON writes both.
pub(crate) fn deep_equal_opt(left: Option<&Json>, right: Option<&Json>) -> bool {
    left.unwrap_or(&Json::Null) == right.unwrap_or(&Json::Null)
}

/// The errors TypeBox reports for a missing value (`undefined`): no JSON value stands for it.
fn undefined_errors(schema: &serde_json::Value) -> Vec<SchemaError> {
    if pi_schema::check_undefined(schema) {
        return Vec::new();
    }
    let error = |keyword: &'static str, message: String| SchemaError {
        keyword,
        instance_path: String::new(),
        message,
        required: Vec::new(),
    };
    let mut errors = Vec::new();
    match schema.get("type") {
        Some(serde_json::Value::String(kind)) => errors.push(error("type", format!("must be {kind}"))),
        Some(serde_json::Value::Array(kinds)) => {
            let kinds: Vec<&str> = kinds.iter().filter_map(serde_json::Value::as_str).collect();
            errors.push(error("type", format!("must be either {}", kinds.join(" or "))));
        }
        _ => {}
    }
    if schema.get("const").is_some() {
        errors.push(error("const", "must be equal to constant".into()));
    }
    if schema.get("enum").is_some() {
        errors.push(error("enum", "must be equal to one of the allowed values".into()));
    }
    if schema.get("anyOf").is_some() {
        errors.push(error("anyOf", "must match a schema in anyOf".into()));
    }
    if errors.is_empty() {
        errors.push(error("schema", "must not be undefined".into()));
    }
    errors
}

/// Fails with `invalid` (with `reason`) and the first schema errors when `value` does not match.
/// `None` is a missing value (`undefined`).
pub fn assert_valid(reason: &str, what: &str, schema: &Json, value: Option<&Json>) -> ChordResult<()> {
    let schema = schema.to_serde();
    let errors = match value {
        Some(value) => {
            let value = value.to_serde();
            let mut checker = pi_schema::Checker::new(&schema);
            if checker.check(&value, &schema, "") {
                return Ok(());
            }
            checker.errors
        }
        None => undefined_errors(&schema),
    };
    if errors.is_empty() {
        return Ok(());
    }
    let shown = &errors[..errors.len().min(3)];
    let summary: Vec<String> = shown
        .iter()
        .map(|error| {
            let path = if error.instance_path.is_empty() { "/" } else { &error.instance_path };
            format!("{path} {}", error.message)
        })
        .collect();
    let details = shown
        .iter()
        .map(|error| {
            let mut entry = JsonMap::new();
            entry.insert("keyword", Json::from(error.keyword));
            entry.insert("instancePath", Json::from(error.instance_path.as_str()));
            entry.insert("message", Json::from(error.message.as_str()));
            Json::object(entry)
        })
        .collect();
    Err(invalid(reason, format!("{what}: {}", summary.join("; ")), Some(Json::array(details))))
}

/// Applies defaults, strips unknown fields, validates. A missing value or `null` counts as `{}`.
pub fn normalize(schema: &Json, value: Option<&Json>, what: &str) -> ChordResult<Json> {
    let start = match value {
        None | Some(Json::Null) => serde_json::Value::Object(serde_json::Map::new()),
        Some(value) => value.to_serde(),
    };
    let target = schema.to_serde();
    let normalized = pi_schema::default_value(&target, Some(start)).map(|value| pi_schema::clean(&target, value));
    let normalized = normalized.map(Json::from);
    assert_valid("input", what, schema, normalized.as_ref())?;
    Ok(normalized.unwrap_or_default())
}

/// Canonical identity of a normalized value: fields equal to their schema default are omitted.
pub fn canonical_identity(schema: &Json, value: &Json) -> String {
    canonical_json(&strip_defaults(&as_schema(Some(schema)), value))
}

/// A schema as an object: `false` is `{ not: {} }`, an array its indices, anything else `{}`.
pub(crate) fn as_schema(schema: Option<&Json>) -> Cow<'_, JsonMap> {
    match schema {
        Some(Json::Object(map)) => Cow::Borrowed(map),
        Some(Json::Bool(false)) => Cow::Owned(JsonMap::from_iter([("not", Json::empty_object())])),
        Some(Json::Array(items)) => {
            Cow::Owned(items.iter().enumerate().map(|(index, item)| (index.to_string(), item.clone())).collect())
        }
        _ => Cow::Owned(JsonMap::new()),
    }
}

fn strip_defaults(schema: &JsonMap, value: &Json) -> Json {
    match value {
        Json::Array(items) => {
            let stripped = match schema.get("items") {
                Some(Json::Array(tuple)) => items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| strip_defaults(&as_schema(tuple.get(index)), item))
                    .collect(),
                other => {
                    let items_schema = as_schema(other);
                    items.iter().map(|item| strip_defaults(&items_schema, item)).collect()
                }
            };
            Json::array(stripped)
        }
        Json::Object(object) => {
            let properties = as_schema(schema.get("properties"));
            let mut result = JsonMap::new();
            for (key, item) in object.iter() {
                let property = as_schema(properties.get(key));
                if property.get("default").is_some_and(|default| deep_equal(default, item)) {
                    continue;
                }
                result.insert(key.clone(), strip_defaults(&property, item));
            }
            Json::object(result)
        }
        _ => value.clone(),
    }
}
