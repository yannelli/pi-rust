//! TypeBox-compatible JSON Schema handling for pi-ai and pi-chord: checks with TypeBox's keyword
//! order and `en_US` error messages, and TypeBox's `Value.Default` and `Value.Clean`.

mod check;
mod clean;
mod default;
mod formats;
mod kind;

use serde_json::Value;

pub use check::{Checker, SchemaError, js_number, json_equal, matches_type};
pub use clean::clean;
pub use default::default_value;

/// TypeBox `Value.Check(schema, value)`.
pub fn check(schema: &Value, value: &Value) -> bool {
    Checker::new(schema).check(value, schema, "")
}

/// TypeBox `Value.Check(schema, undefined)`: whether the schema accepts a missing value.
pub fn check_undefined(schema: &Value) -> bool {
    let Value::Object(map) = schema else { return schema != &Value::Bool(false) };
    let types_accept = match map.get("type") {
        Some(Value::String(kind)) => matches!(kind.as_str(), "undefined" | "void"),
        Some(Value::Array(kinds)) => kinds.iter().any(|kind| matches!(kind.as_str(), Some("undefined" | "void"))),
        _ => true,
    };
    let branches =
        |keyword: &str| map.get(keyword).and_then(Value::as_array).map(|items| items.iter().map(check_undefined));
    types_accept
        && !map.contains_key("const")
        && !map.contains_key("enum")
        && branches("anyOf").is_none_or(|mut results| results.any(|accepted| accepted))
        && branches("allOf").is_none_or(|mut results| results.all(|accepted| accepted))
        && branches("oneOf").is_none_or(|results| results.filter(|accepted| *accepted).count() == 1)
        && map.get("not").is_none_or(|negated| !check_undefined(negated))
}

/// JavaScript `new RegExp(pattern, "u").test(text)`; `None` when the pattern does not compile.
pub fn pattern_test(pattern: &str, text: &str) -> Option<bool> {
    regex::Regex::new(pattern).ok().map(|regex| regex.is_match(text))
}

/// TypeBox's error path: `/a/0/b` becomes `a.0.b`; `required` names the first missing property.
pub fn display_path(error: &SchemaError) -> String {
    let base = error.instance_path.trim_start_matches('/').replace('/', ".");
    if error.keyword == "required"
        && let Some(missing) = error.required.first()
    {
        return if base.is_empty() { missing.clone() } else { format!("{base}.{missing}") };
    }
    if base.is_empty() { "root".into() } else { base }
}

/// `value` checked against `schema`: one `path: message` line per error, empty when valid.
pub fn schema_errors(schema: &Value, value: &Value) -> Vec<String> {
    let mut checker = Checker::new(schema);
    if checker.check(value, schema, "") {
        return Vec::new();
    }
    checker.errors.iter().map(|error| format!("{}: {}", display_path(error), error.message)).collect()
}
