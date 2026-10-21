//! What a script gets for its calls, how the journal stores a global's outcome, and the tool result of a
//! finished script.

use pi_ai::ContentBlock;
use pi_chord::Json;
use pi_codemode::js::js_stringify;
use pi_codemode::sandbox::{
    CodemodeCallTarget, CodemodeErrorKind, CodemodeHostCall, CodemodeOutcome, CodemodeOutputItem, CodemodeResult,
};
use serde_json::{Map, Value, json};

use super::docs::StoredResult;
use crate::harness::{DiagnosticSeverity, NestedToolExecutionResult};

/// What a script gets for a nested call: success resolves with its value; every error rejects (design section 4.5).
pub(super) fn outcome_of(name: &str, result: &NestedToolExecutionResult) -> CodemodeOutcome {
    let value = result.structured_output.as_ref().map(Value::from);
    if !result.is_error {
        return CodemodeOutcome::Ok(value);
    }
    let message = match &value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items.iter().filter_map(text_of_block).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    };
    let diagnostics: Vec<&str> = result
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error)
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();
    let diagnostics = diagnostics.join("\n");
    let message = [message, diagnostics].into_iter().find(|text| !text.is_empty());
    CodemodeOutcome::Err {
        message: message.unwrap_or_else(|| format!("Tool {name} failed")),
        properties: value.map(|value| Map::from_iter([("structuredOutput".to_owned(), value)])),
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

pub(super) fn kind_name(kind: CodemodeErrorKind) -> &'static str {
    match kind {
        CodemodeErrorKind::Script => "script",
        CodemodeErrorKind::Timeout => "timeout",
        CodemodeErrorKind::Aborted => "aborted",
        CodemodeErrorKind::Sandbox => "sandbox",
    }
}

/// The tool result for a script's result.
pub(super) fn tool_result(result: &CodemodeResult) -> StoredResult {
    let mut output: Vec<ContentBlock> = result
        .output
        .iter()
        .map(|item| match item {
            CodemodeOutputItem::Image { data, mime_type } => {
                ContentBlock::Image { data: data.clone(), mime_type: mime_type.clone() }
            }
            CodemodeOutputItem::Text { text, .. } => ContentBlock::text(text.clone()),
        })
        .collect();
    match &result.outcome {
        Ok(success) => {
            if let Some(value) = &success.value {
                let text = match value {
                    Value::String(text) => text.clone(),
                    other => js_stringify(other),
                };
                output.push(ContentBlock::text(text));
            }
        }
        Err(error) => {
            let text = error.stack.clone().unwrap_or_else(|| error.message.clone());
            output.push(ContentBlock::text(format!("Script error ({}): {text}", kind_name(error.kind))));
        }
    }
    StoredResult { output, is_error: !result.is_ok(), details: None }
}
