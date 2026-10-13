//! Tool results: error results the Harness writes, bounding, diagnostics, and the `pi.tool-result` entry.

use pi_ai::{ContentBlock, Message, ToolCall, ToolResultMessage};
use pi_chord::Json;

use crate::documents::encode;
use crate::entries::{TOOL_RESULT_ENTRY, ToolResultData, TypedEntryDraft};
use crate::errors::{DurableError, DurableResult};
use crate::harness::live::{NestedToolSummary, SlotProgress};
use crate::harness::output::{OutputLimits, bound_output};
use crate::harness::tool_registration::{Retain, ToolRegistration};
use crate::harness::types::{DiagnosticSeverity, NestedToolExecutionResult, ToolDiagnostic, ToolExecutionResult};
use crate::harness::usage::{UsageBucket, record_usage};
use crate::ids::{ConversationId, TaskId};
use crate::session::Tx;
use crate::types::{EntryRecord, TaskOutcome};

/// An error result the Harness writes itself: no content and one `error` diagnostic with `code`.
pub fn harness_error(code: &str, message: &str) -> ToolExecutionResult {
    ToolExecutionResult {
        output: Some(Vec::new()),
        is_error: Some(true),
        diagnostics: Some(vec![tool_diagnostic(code, message)]),
        ..ToolExecutionResult::default()
    }
}

pub(crate) fn tool_diagnostic(code: &str, message: &str) -> ToolDiagnostic {
    ToolDiagnostic::new(DiagnosticSeverity::Error, message).with_code(code)
}

/// The Harness's truncation diagnostic; the kept end is unknown when rebuilt from a slot after recovery.
pub(crate) fn truncated(dropped_bytes: u64, dropped_lines: u64, kept: Option<Retain>) -> ToolDiagnostic {
    let kept = match kept {
        None => "",
        Some(Retain::Head) => " to its beginning",
        Some(Retain::Tail) => " to its end",
    };
    let message = format!("Output truncated{kept}: {dropped_lines} lines, {dropped_bytes} bytes dropped");
    ToolDiagnostic::new(DiagnosticSeverity::Warn, message).with_code("truncated")
}

/// An error result from the slot's durable partial output, details, and diagnostics.
pub(crate) fn from_slot(slot: Option<SlotProgress>, code: &str, message: &str) -> ToolExecutionResult {
    let slot = slot.unwrap_or_default();
    let mut diagnostics = slot.diagnostics;
    if slot.dropped_bytes > 0 {
        diagnostics.push(truncated(slot.dropped_bytes, slot.dropped_lines, None));
    }
    diagnostics.push(tool_diagnostic(code, message));
    let output = match slot.output {
        Some(text) if !text.is_empty() => vec![ContentBlock::text(text)],
        _ => Vec::new(),
    };
    ToolExecutionResult {
        output: Some(output),
        is_error: Some(true),
        details: slot.details,
        diagnostics: Some(diagnostics),
        ..ToolExecutionResult::default()
    }
}

fn render_diagnostics(diagnostics: &[ToolDiagnostic]) -> String {
    let lines: Vec<String> = diagnostics.iter().map(|d| format!("[{}] {}", d.severity.as_str(), d.message)).collect();
    format!("<harness>\n{}\n</harness>", lines.join("\n"))
}

/// Append a `pi.tool-result` entry whose content ends with the rendered diagnostics; `data` keeps the list.
pub async fn append_tool_result(
    tx: &Tx,
    conversation_id: ConversationId,
    call: &ToolCall,
    result: &ToolExecutionResult,
    timestamp: f64,
    duration_ms: Option<u64>,
) -> DurableResult<EntryRecord> {
    let diagnostics = result.diagnostics.clone().unwrap_or_default();
    let mut content = result.output.clone().unwrap_or_default();
    if !diagnostics.is_empty() {
        content.push(ContentBlock::text(render_diagnostics(&diagnostics)));
    }
    let message = ToolResultMessage {
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        content,
        details: result.details.as_ref().map(Json::to_serde),
        usage: result.usage,
        nested_calls: None,
        is_error: result.is_error.unwrap_or(false),
        timestamp: timestamp as i64,
        duration_ms,
    };
    if let Some(usage) = &result.usage {
        record_usage(tx, conversation_id, UsageBucket::Tools, &call.name, usage).await?;
    }
    let data = ToolResultData { diagnostics };
    let draft = TypedEntryDraft { model: Some(vec![Message::ToolResult(message)]), ..TypedEntryDraft::new(data) };
    Ok(tx.append_typed_entry(&TOOL_RESULT_ENTRY, conversation_id, draft).await?.record)
}

/// Bounded content: when the joined text exceeds the limits, the text items become one bounded item at the
/// position of the first (head) or last (tail) text item; other content stays.
pub(crate) fn bound_content(content: &[ContentBlock], limits: OutputLimits) -> (Vec<ContentBlock>, u64, u64) {
    let texts: Vec<(usize, &str)> =
        content.iter().enumerate().filter_map(|(index, item)| item.as_text().map(|text| (index, text))).collect();
    let joined: String = texts.iter().map(|(_, text)| *text).collect();
    let bounded = bound_output(&joined, limits);
    if bounded.dropped_bytes == 0 {
        return (content.to_vec(), 0, 0);
    }
    let keep = match limits.retain {
        Retain::Head => texts.first().map(|(index, _)| *index),
        Retain::Tail => texts.last().map(|(index, _)| *index),
    };
    let mut result = Vec::new();
    for (index, item) in content.iter().enumerate() {
        match item {
            ContentBlock::Text { text_signature, .. } if Some(index) == keep => {
                result.push(ContentBlock::Text { text: bounded.text.clone(), text_signature: text_signature.clone() })
            }
            ContentBlock::Text { .. } => {}
            other => result.push(other.clone()),
        }
    }
    (result, bounded.dropped_bytes, bounded.dropped_lines)
}

/// What programs get of a tool's output without a schema: one text as its string, one image as itself,
/// nothing as `""`, and anything else as the content list.
pub(crate) fn output_value(output: &[ContentBlock]) -> DurableResult<Json> {
    match output {
        [] => Ok(Json::from("")),
        [ContentBlock::Text { text, .. }] => Ok(Json::from(text.as_str())),
        [only] => encode(only),
        many => encode(many),
    }
}

/// Why a result's `structured_output` breaks the tool's contract, or `None`.
pub(crate) fn structured_output_error(tool: &ToolRegistration, result: &ToolExecutionResult) -> Option<String> {
    let name = tool.name();
    let Some(schema) = &tool.structured_output_schema else {
        let message = format!("Tool {name} returned structuredOutput but declares no structuredOutputSchema");
        return result.structured_output.as_ref().map(|_| message);
    };
    let Some(value) = &result.structured_output else {
        return (result.is_error != Some(true)).then(|| format!("Tool {name} returned no structuredOutput"));
    };
    let errors = pi_ai::json_schema::schema_errors(schema, &value.to_serde());
    let first = errors.first()?;
    Some(format!("Tool {name} returned structuredOutput that does not match its schema: {first}"))
}

/// A nested call's result, as stored and returned: without `output` and `control`.
pub(crate) fn nested_result(
    task_id: TaskId,
    result: &ToolExecutionResult,
    duration_ms: Option<u64>,
) -> NestedToolExecutionResult {
    NestedToolExecutionResult {
        task_id,
        structured_output: result.structured_output.clone(),
        is_error: result.is_error.unwrap_or(false),
        details: result.details.clone(),
        diagnostics: result.diagnostics.clone().unwrap_or_default(),
        usage: result.usage,
        duration_ms,
    }
}

/// What the caller gets for a nested call without a stored result: one the scheduler faulted or orphaned.
pub(crate) fn fallback_result(task_id: TaskId, name: &str, outcome: Option<&TaskOutcome>) -> NestedToolExecutionResult {
    let (code, message) = match outcome {
        Some(TaskOutcome::Faulted { error }) => ("faulted", format!("Tool {name} failed: {}", error.message)),
        Some(TaskOutcome::Orphaned { reason }) => ("orphaned", format!("Tool {name} could not resume: {reason}")),
        Some(other) => (other.status(), format!("Tool {name} ended without a result")),
        None => ("faulted", format!("Tool {name} ended without a result")),
    };
    NestedToolExecutionResult {
        task_id,
        structured_output: None,
        is_error: true,
        details: None,
        diagnostics: vec![tool_diagnostic(code, &message)],
        usage: None,
        duration_ms: None,
    }
}

const MAX_SUMMARY_ERROR_CHARS: usize = 500;

/// How a nested call ended, for its slot: error text, from its diagnostics or else its output, bounded.
pub(crate) fn summary_of(result: &NestedToolExecutionResult, output: &[ContentBlock]) -> NestedToolSummary {
    let error = if result.is_error {
        let errors: Vec<&str> = result
            .diagnostics
            .iter()
            .filter(|d| d.severity == DiagnosticSeverity::Error)
            .map(|d| d.message.as_str())
            .collect();
        let text = if errors.is_empty() {
            output.iter().filter_map(ContentBlock::as_text).collect::<Vec<_>>().join("\n")
        } else {
            errors.join("\n")
        };
        text.chars().take(MAX_SUMMARY_ERROR_CHARS).collect()
    } else {
        String::new()
    };
    NestedToolSummary {
        is_error: result.is_error,
        duration_ms: result.duration_ms,
        usage: result.usage,
        error: (!error.is_empty()).then_some(error),
    }
}

/// Explicit nested call keys are path segments of the call ID and never plain positive integers.
pub(crate) fn check_key(key: &str) -> DurableResult<()> {
    let integer = key.starts_with(|c: char| ('1'..='9').contains(&c)) && key.chars().all(|c| c.is_ascii_digit());
    if key.is_empty() || key.contains('/') || key == "__proto__" || integer {
        let quoted = Json::from(key).to_string();
        return Err(DurableError::error(format!(
            "Nested call key {quoted} must be non-empty, without \"/\", not \"__proto__\", and not a positive integer"
        )));
    }
    Ok(())
}
