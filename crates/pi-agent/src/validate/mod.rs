//! Tool argument validation (upstream `validateToolArguments`): the arguments are coerced first,
//! then checked, with pi-schema's error messages.

use serde_json::Value;

use pi_ai::json_schema::{Checker, parse};

/// Whether `value` matches `schema`.
pub fn is_valid(schema: &Value, value: &Value) -> bool {
    Checker::new(schema).is_valid(value, schema)
}

/// A pi-schema error path (a JSON Pointer) as the dotted path the model sees: `/edits/0/oldText`
/// becomes `edits.0.oldText`.
fn format_validation_path(pointer: &str) -> String {
    if pointer.is_empty() {
        return "root".into();
    }
    let segments: Vec<String> =
        pointer[1..].split('/').map(|segment| segment.replace("~1", "/").replace("~0", "~")).collect();
    segments.join(".")
}

/// Coerce `arguments` toward `schema` and validate them: models often send `"42"` for a number,
/// a single value for an array, or `null` for an omitted optional property. The error text
/// matches the TypeScript pi format, so models see the same feedback.
pub fn validate_tool_arguments(tool_name: &str, schema: &Value, arguments: &Value) -> Result<Value, String> {
    let errors = match parse(schema, arguments) {
        Ok(value) => return Ok(value),
        Err(errors) => errors,
    };
    let errors: Vec<String> =
        errors.iter().map(|error| format!("  - {}: {}", format_validation_path(&error.path), error.message)).collect();
    Err(format!(
        "Validation failed for tool \"{tool_name}\":\n{}\n\nReceived arguments:\n{}",
        errors.join("\n"),
        serde_json::to_string_pretty(arguments).unwrap_or_default()
    ))
}

#[cfg(test)]
mod tests;
