//! What a script gets for its calls, and how the journal stores a global's outcome.

use pi_chord::Json;
use pi_codemode::js::js_stringify;
use pi_codemode::sandbox::{CodemodeCallTarget, CodemodeHostCall, CodemodeOutcome};
use serde_json::{Map, Value, json};

use crate::harness::{DiagnosticSeverity, NestedToolExecutionResult};

/// What a script gets for a nested call, as in the coding agent. A tool with a schema resolves with its value, also on
/// an error result that carries one; a schema-less tool resolves with its text output, images left out. Any other
/// error rejects, with its text: a schema-less tool's output, else its error diagnostics.
pub(super) fn outcome_of(name: &str, structured: bool, result: &NestedToolExecutionResult) -> CodemodeOutcome {
    let stored = result.structured_output.as_ref().map(Value::from);
    let value = if structured { stored } else { Some(Value::String(text_of(stored))) };
    if !result.is_error || (structured && value.is_some()) {
        return CodemodeOutcome::Ok(value);
    }
    let message = match (&value, structured) {
        (Some(Value::String(text)), false) => text.clone(),
        _ => String::new(),
    };
    let message = [message, error_diagnostics(result)].into_iter().find(|text| !text.is_empty());
    CodemodeOutcome::Err {
        message: message.unwrap_or_else(|| format!("Tool {name} failed")),
        properties: value.map(|value| Map::from_iter([("structuredOutput".to_owned(), value)])),
    }
}

/// The messages of a nested result's error diagnostics, one per line.
pub(super) fn error_diagnostics(result: &NestedToolExecutionResult) -> String {
    let errors = result.diagnostics.iter().filter(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error);
    errors.map(|diagnostic| diagnostic.message.as_str()).collect::<Vec<_>>().join("\n")
}

/// The text of a schema-less tool's collapsed output: a string, one image, or an array of text and image blocks.
fn text_of(value: Option<Value>) -> String {
    match value {
        Some(Value::String(text)) => text,
        Some(Value::Array(items)) => items.iter().filter_map(text_of_block).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

fn text_of_block(item: &Value) -> Option<String> {
    let item = item.as_object()?;
    (item.get("type")? == "text").then(|| item.get("text").and_then(Value::as_str).unwrap_or_default().to_owned())
}

/// A global's outcome as the journal stores it: `{ ok: true, value? }` or `{ ok: false, message, properties? }`.
pub(super) fn outcome_json(outcome: &CodemodeOutcome) -> Value {
    match outcome {
        CodemodeOutcome::Ok(None) => json!({ "ok": true }),
        CodemodeOutcome::Ok(Some(value)) => json!({ "ok": true, "value": value }),
        CodemodeOutcome::Err { message, properties: None } => json!({ "ok": false, "message": message }),
        CodemodeOutcome::Err { message, properties: Some(properties) } => {
            json!({ "ok": false, "message": message, "properties": properties })
        }
    }
}

/// The outcome a journaled global's entry stores.
pub(super) fn outcome_from_json(value: &Value) -> CodemodeOutcome {
    if value.get("ok") == Some(&Value::Bool(true)) {
        return CodemodeOutcome::Ok(value.get("value").cloned());
    }
    CodemodeOutcome::Err {
        message: value.get("message").and_then(Value::as_str).unwrap_or_default().to_owned(),
        properties: value.get("properties").and_then(Value::as_object).cloned(),
    }
}

/// `tools.echo({"text":"a"})` or `next()`.
pub(super) fn describe(call: &CodemodeHostCall) -> String {
    let prefix = if call.target == CodemodeCallTarget::Tool { "tools." } else { "" };
    let args = call.args.as_ref().map(js_stringify).unwrap_or_default();
    format!("{prefix}{}({args})", call.name)
}

/// Structural JSON equality; key order does not matter.
pub(super) fn same_json(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => Json::from(a) == Json::from(b),
        _ => false,
    }
}
