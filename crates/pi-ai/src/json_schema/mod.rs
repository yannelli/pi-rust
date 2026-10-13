//! JSON Schema checks with TypeBox's keyword order and `en_US` error messages, shared by tool
//! argument validation and the `models.json` schema check.

mod check;
mod formats;

use serde_json::Value;

pub use check::{Checker, SchemaError, js_number, json_equal, matches_type};

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
