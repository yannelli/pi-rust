//! `edit` tool: exact text replacement with uniqueness checks and a diff in `details`.

use std::io::ErrorKind;
use std::path::PathBuf;

use anyhow::{anyhow, bail};
use async_trait::async_trait;
use pi_agent::{AgentTool, AgentToolResult, ToolUpdateCallback};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::edit_diff::{
    Edit, apply_edits_to_normalized_content, detect_line_ending, generate_diff_string, generate_unified_patch,
    normalize_to_lf, restore_line_endings,
};
use super::mutation_queue::with_file_mutation_queue;
use super::path_utils::resolve_to_cwd;
use super::{check_aborted, parse_args};

const BOM: char = '\u{FEFF}';

pub struct EditTool {
    cwd: PathBuf,
}

#[derive(Deserialize)]
struct EditArgs {
    path: String,
    #[serde(default)]
    edits: Vec<Edit>,
}

impl EditTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

fn is_single_edit(value: &Value) -> bool {
    value.get("oldText").is_some_and(Value::is_string) && value.get("newText").is_some_and(Value::is_string)
}

/// Accept edits sent as a JSON string, a single edit object, or legacy top-level oldText/newText.
fn prepare_edit_arguments(mut args: Value) -> Value {
    let Some(obj) = args.as_object_mut() else {
        return args;
    };
    match obj.get("edits") {
        Some(Value::String(raw)) => {
            if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
                if parsed.is_array() {
                    obj.insert("edits".into(), parsed);
                } else if is_single_edit(&parsed) {
                    obj.insert("edits".into(), Value::Array(vec![parsed]));
                }
            }
        }
        Some(single) if is_single_edit(single) => {
            let single = single.clone();
            obj.insert("edits".into(), Value::Array(vec![single]));
        }
        _ => {}
    }
    let legacy = Value::Object(obj.clone());
    if is_single_edit(&legacy) {
        let mut edits = obj.get("edits").and_then(Value::as_array).cloned().unwrap_or_default();
        edits.push(json!({ "oldText": legacy["oldText"], "newText": legacy["newText"] }));
        obj.remove("oldText");
        obj.remove("newText");
        obj.insert("edits".into(), Value::Array(edits));
    }
    args
}

fn error_code(kind: ErrorKind) -> String {
    match kind {
        ErrorKind::NotFound => "ENOENT".into(),
        ErrorKind::PermissionDenied => "EACCES".into(),
        ErrorKind::IsADirectory => "EISDIR".into(),
        other => format!("{other:?}"),
    }
}

#[async_trait]
impl AgentTool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to edit (relative or absolute)" },
                "edits": {
                    "type": "array",
                    "description": "One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "oldText": {
                                "type": "string",
                                "description": "Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call."
                            },
                            "newText": { "type": "string", "description": "Replacement text for this targeted edit." }
                        },
                        "required": ["oldText", "newText"]
                    }
                }
            },
            "required": ["path", "edits"]
        })
    }

    fn prepare_arguments(&self, args: Value) -> anyhow::Result<Value> {
        Ok(prepare_edit_arguments(args))
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        _on_update: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        let args: EditArgs = parse_args("edit", prepare_edit_arguments(args))?;
        if args.edits.is_empty() {
            bail!("Edit tool input is invalid. edits must contain at least one replacement.");
        }
        let path = args.path.as_str();
        let absolute = resolve_to_cwd(path, &self.cwd);

        with_file_mutation_queue(&absolute, || async {
            check_aborted(&cancel)?;
            let meta = tokio::fs::metadata(&absolute)
                .await
                .map_err(|e| anyhow!("Could not edit file: {path}. Error code: {}.", error_code(e.kind())))?;
            if meta.is_dir() {
                bail!("Could not edit file: {path}. Error code: EISDIR.");
            }
            if meta.permissions().readonly() {
                bail!("Could not edit file: {path}. Error code: EACCES.");
            }
            let buffer = tokio::fs::read(&absolute)
                .await
                .map_err(|e| anyhow!("Could not edit file: {path}. Error code: {}.", error_code(e.kind())))?;
            check_aborted(&cancel)?;

            let raw = String::from_utf8_lossy(&buffer);
            let (bom, content) = match raw.strip_prefix(BOM) {
                Some(rest) => (BOM.to_string(), rest),
                None => (String::new(), raw.as_ref()),
            };
            let ending = detect_line_ending(content);
            let normalized = normalize_to_lf(content);
            let (base, new_content) = apply_edits_to_normalized_content(&normalized, &args.edits, path)?;
            check_aborted(&cancel)?;

            let final_content = format!("{bom}{}", restore_line_endings(&new_content, ending));
            tokio::fs::write(&absolute, final_content.as_bytes())
                .await
                .map_err(|e| anyhow!("Could not write file: {path}. {e}"))?;
            check_aborted(&cancel)?;

            let (diff, first_changed_line) = generate_diff_string(&base, &new_content);
            let patch = generate_unified_patch(path, &base, &new_content);
            let mut details = json!({ "diff": diff, "patch": patch });
            if let Some(line) = first_changed_line {
                details["firstChangedLine"] = json!(line);
            }
            let text = format!("Successfully replaced {} block(s) in {path}.", args.edits.len());
            Ok(AgentToolResult::text(text).with_details(details))
        })
        .await
    }
}
