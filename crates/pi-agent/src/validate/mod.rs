//! Tool argument coercion and validation (upstream `validateToolArguments`): drop optional
//! nulls, coerce toward the schema, then check it with TypeBox's keywords and messages.

mod check;
mod coerce;
mod formats;

use serde_json::Value;

use check::{Checker, SchemaError};
use coerce::{coerce, normalize_optional_nulls};

/// TypeBox's error path: `/a/0/b` becomes `a.0.b`; `required` names the first missing property.
fn display_path(error: &SchemaError) -> String {
    let base = error.instance_path.trim_start_matches('/').replace('/', ".");
    if error.keyword == "required"
        && let Some(missing) = error.required.first()
    {
        return if base.is_empty() { missing.clone() } else { format!("{base}.{missing}") };
    }
    if base.is_empty() { "root".into() } else { base }
}

/// Whether `value` matches `schema`.
pub fn is_valid(schema: &Value, value: &Value) -> bool {
    Checker::new(schema).is_valid(value, schema)
}

/// Coerce `arguments` toward `schema` and validate them. The error text matches the TypeScript
/// pi format, so models see the same feedback.
pub fn validate_tool_arguments(tool_name: &str, schema: &Value, arguments: &Value) -> Result<Value, String> {
    let checker = Checker::new(schema);
    let mut value = arguments.clone();
    normalize_optional_nulls(&mut value, schema, &checker);
    let value = coerce(value, schema, &checker);
    let mut checker = Checker::new(schema);
    if checker.check(&value, schema, "") {
        return Ok(value);
    }
    let errors: Vec<String> =
        checker.errors.iter().map(|error| format!("  - {}: {}", display_path(error), error.message)).collect();
    let errors = if errors.is_empty() { "Unknown validation error".to_string() } else { errors.join("\n") };
    Err(format!(
        "Validation failed for tool \"{tool_name}\":\n{errors}\n\nReceived arguments:\n{}",
        serde_json::to_string_pretty(arguments).unwrap_or_default()
    ))
}

#[cfg(test)]
mod tests;
