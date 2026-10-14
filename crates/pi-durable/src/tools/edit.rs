//! The `edit` tool: exact-text replacements in one file, keeping its BOM and line endings.

use std::sync::Arc;

use pi_ai::JsonObject;
use pi_chord::Context;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::edit_diff::{
    Edit, apply_edits_to_normalized_content, detect_line_ending, generate_diff_string, generate_unified_patch,
    normalize_to_lf, restore_line_endings, strip_bom,
};
use super::file_mutation_queue::with_file_mutation_queue;
use super::path_utils::resolve_tool_path;
use super::{ToolCallApi, check_aborted, parse_args, require_env};
use crate::env::{FileError, FileKind};
use crate::errors::{DurableError, DurableResult};
use crate::harness::{ToolExecutionResult, ToolRegistration, define_tool};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditToolInput {
    pub path: String,
    pub edits: Vec<Edit>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditToolDetails {
    pub diff: String,
    pub patch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_changed_line: Option<usize>,
}

fn single_edit(value: &Value) -> bool {
    value.get("oldText").is_some_and(Value::is_string) && value.get("newText").is_some_and(Value::is_string)
}

/// Repair shapes models commonly send: `edits` as a JSON string or as a single edit object, and a top-level
/// `oldText`/`newText` pair. Works on a copy; the call's arguments stay unchanged.
pub fn prepare_edit_arguments(input: &Value) -> Value {
    let Some(object) = input.as_object() else { return input.clone() };
    let mut args = object.clone();
    match args.get("edits") {
        Some(Value::String(text)) => match serde_json::from_str::<Value>(text) {
            Ok(parsed @ Value::Array(_)) => _ = args.insert("edits".into(), parsed),
            Ok(parsed) if single_edit(&parsed) => _ = args.insert("edits".into(), Value::Array(vec![parsed])),
            _ => {}
        },
        Some(edit) if single_edit(edit) => {
            let edit = edit.clone();
            args.insert("edits".into(), Value::Array(vec![edit]));
        }
        _ => {}
    }
    let (Some(Value::String(old)), Some(Value::String(new))) = (args.get("oldText"), args.get("newText")) else {
        return Value::Object(args);
    };
    let pair = json!({ "oldText": old, "newText": new });
    let mut edits = match args.get("edits") {
        Some(Value::Array(edits)) => edits.clone(),
        _ => Vec::new(),
    };
    edits.push(pair);
    args.remove("oldText");
    args.remove("newText");
    args.insert("edits".into(), Value::Array(edits));
    Value::Object(args)
}

fn edit_access_error(path: &str, error: FileError) -> DurableError {
    DurableError::error(format!("Could not edit file: {path}. Error code: {}.", error.code.as_str()))
}

pub fn create_edit_tool() -> ToolRegistration {
    let edit = json!({
        "type": "object",
        "properties": {
            "oldText": { "type": "string", "description": "Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call." },
            "newText": { "type": "string", "description": "Replacement text for this targeted edit." }
        },
        "required": ["oldText", "newText"]
    });
    define_tool(
        "edit",
        "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes.",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to edit (relative or absolute)" },
                "edits": { "type": "array", "items": edit, "description": "One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead." }
            },
            "required": ["path", "edits"]
        }),
        |args, api, context| execute_edit(args, Arc::new(api), context),
    )
    .with_prepare_arguments(|input| Ok(prepare_edit_arguments(input)))
}

/// The `edit` tool's execution against any call api.
pub async fn execute_edit(
    args: JsonObject,
    api: Arc<dyn ToolCallApi>,
    context: Context,
) -> DurableResult<ToolExecutionResult> {
    let EditToolInput { path, edits } = parse_args(args)?;
    if edits.is_empty() {
        return Err(DurableError::error("Edit tool input is invalid. edits must contain at least one replacement."));
    }
    let env = require_env(api.as_ref())?;
    let absolute = resolve_tool_path(env.as_ref(), &path, &context).await?;
    let mutate = || async {
        check_aborted(&context)?;
        let info = env.file_info(&absolute, &context).await.map_err(|error| edit_access_error(&path, error))?;
        if !matches!(info.kind, FileKind::File | FileKind::Symlink) {
            return Err(DurableError::error(format!("Could not edit file: {path}. Path is not a file.")));
        }
        let text = env.read_text_file(&absolute, &context).await.map_err(|error| edit_access_error(&path, error))?;
        check_aborted(&context)?;

        let (bom, content) = strip_bom(&text);
        let ending = detect_line_ending(content);
        let normalized = normalize_to_lf(content);
        let (base, new_content) =
            apply_edits_to_normalized_content(&normalized, &edits, &path).map_err(DurableError::error)?;
        check_aborted(&context)?;

        let final_content = format!("{bom}{}", restore_line_endings(&new_content, ending));
        env.write_file(&absolute, final_content.as_bytes(), &context)
            .await
            .map_err(|error| edit_access_error(&path, error))?;
        check_aborted(&context)?;

        let (diff, first_changed_line) = generate_diff_string(&base, &new_content);
        let details =
            EditToolDetails { diff, patch: generate_unified_patch(&path, &base, &new_content), first_changed_line };
        Ok(ToolExecutionResult {
            details: Some(serde_json::to_value(details)?.into()),
            ..ToolExecutionResult::text(format!("Successfully replaced {} block(s) in {path}.", edits.len()))
        })
    };
    with_file_mutation_queue(env.as_ref(), &absolute, mutate, &context).await
}
