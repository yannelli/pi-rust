//! Helpers for the chord2 (host2) tests: golden fixtures generated from upstream TypeScript, and schema builders
//! that write what TypeBox's `Type.*` builders produce.
#![allow(dead_code)]

use pi_chord::Json;
use pi_chord::host2::hash64;
use pi_chord::json;

/// A golden fixture from `tests/fixtures/chord2` (see `xtask/chord2-fixtures`).
pub fn fixture(name: &str) -> Json {
    let path = format!("{}/tests/fixtures/chord2/{name}", env!("CARGO_MANIFEST_DIR"));
    Json::parse(&std::fs::read_to_string(path).unwrap()).unwrap()
}

pub fn text(value: &Json) -> &str {
    value.as_str().unwrap_or_else(|| panic!("not a string: {value}"))
}

/// Compares a text with a fixture entry: the text itself, or `{ length, hash }` for a long one.
pub fn assert_digest(actual: &str, expected: &Json, label: &str) {
    match expected {
        Json::String(expected) => assert_eq!(actual, &**expected, "{label}"),
        digest => {
            let length = actual.encode_utf16().count() as f64;
            assert_eq!(Some(length), digest.get("length").and_then(Json::as_f64), "{label}: length");
            assert_eq!(hash64(actual), text(&digest["hash"]), "{label}: hash");
        }
    }
}

pub fn string() -> Json {
    json!({"type": "string"})
}

pub fn number() -> Json {
    json!({"type": "number"})
}

pub fn integer() -> Json {
    json!({"type": "integer"})
}

pub fn boolean() -> Json {
    json!({"type": "boolean"})
}

pub fn unknown() -> Json {
    json!({})
}

pub fn void() -> Json {
    json!({"type": "void"})
}

pub fn literal(value: &str) -> Json {
    json!({"type": "string", "const": value})
}

pub fn array(items: Json) -> Json {
    json!({"type": "array", "items": items.to_serde()})
}

pub fn union(branches: Vec<Json>) -> Json {
    Json::from(serde_json::json!({"anyOf": branches.iter().map(Json::to_serde).collect::<Vec<_>>()}))
}

pub fn function(parameters: Vec<Json>, returns: Json) -> Json {
    let parameters: Vec<_> = parameters.iter().map(Json::to_serde).collect();
    json!({"type": "function", "parameters": parameters, "returnType": returns.to_serde()})
}

/// `Type.Object`: required properties first in `required`, optional ones (`Some(true)`) left out of it.
pub fn object(properties: &[(&str, Json, bool)]) -> Json {
    let mut schema = serde_json::Map::new();
    schema.insert("type".into(), "object".into());
    let required: Vec<_> = properties.iter().filter(|(_, _, optional)| !optional).map(|(name, ..)| *name).collect();
    if !required.is_empty() {
        schema.insert("required".into(), required.into());
    }
    let properties = properties.iter().map(|(name, schema, _)| (name.to_string(), schema.to_serde()));
    schema.insert("properties".into(), serde_json::Value::Object(properties.collect()));
    Json::from(serde_json::Value::Object(schema))
}

/// A required property.
pub fn req(name: &str, schema: Json) -> (&str, Json, bool) {
    (name, schema, false)
}

/// An optional property.
pub fn opt(name: &str, schema: Json) -> (&str, Json, bool) {
    (name, schema, true)
}

/// A schema with extra keywords merged in (`Type.String({ minLength: 1 })`).
pub fn with(schema: Json, extra: serde_json::Value) -> Json {
    let mut schema = schema.to_serde();
    if let (Some(target), serde_json::Value::Object(extra)) = (schema.as_object_mut(), extra) {
        target.extend(extra);
    }
    Json::from(schema)
}
