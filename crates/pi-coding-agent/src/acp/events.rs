//! pi session events as ACP session updates (upstream `PiAcpSession.handlePiEvent`).

use std::path::Path;

use agent_client_protocol_schema::{
    ContentBlock, ContentChunk, Diff, SessionUpdate, TextContent, ToolCall, ToolCallContent, ToolCallLocation,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use serde_json::{Value, json};

use super::bash::{
    bash_command, bash_exit_code, bash_output_delta, bash_result_text, bash_terminal_content, bash_terminal_exit_meta,
    bash_terminal_info_meta, bash_terminal_output_meta, is_bash_tool,
};
use super::session::{AcpSession, Snapshot, ToolPhase, text_chunk};
use super::translate::{tool_kind, tool_result_to_text};
use crate::agent_session::lock;
use crate::session::ids::{iso_timestamp, uuid_v4};

const COMPACTION_START: &str = "Context nearing limit, running automatic compaction...";
const COMPACTION_END: &str = "Automatic compaction finished; context was summarized to continue the session.";

/// The 1-based line of `needle` when it occurs exactly once in `text`.
pub fn find_unique_line_number(text: &str, needle: &str) -> Option<u32> {
    if needle.is_empty() {
        return None;
    }
    let first = text.find(needle)?;
    if text[first + needle.len()..].contains(needle) {
        return None;
    }
    Some(1 + text[..first].matches('\n').count() as u32)
}

fn tool_path(args: &Value) -> Option<&str> {
    args.get("path").and_then(Value::as_str).or_else(|| args.get("file_path").and_then(Value::as_str))
}

/// The edit's `oldText` values: top level, then `edits` (also as a JSON string).
pub fn edit_old_texts(args: &Value) -> Vec<String> {
    let mut texts: Vec<String> = Vec::new();
    if let Some(old) = args.get("oldText").and_then(Value::as_str) {
        texts.push(old.to_string());
    }
    let edits = match args.get("edits") {
        Some(Value::String(text)) => serde_json::from_str(text).unwrap_or(Value::Null),
        Some(edits) => edits.clone(),
        None => Value::Null,
    };
    for edit in edits.as_array().into_iter().flatten() {
        if let Some(old) = edit.get("oldText").and_then(Value::as_str)
            && !texts.iter().any(|text| text == old)
        {
            texts.push(old.to_string());
        }
    }
    texts
}

fn resolve(cwd: &Path, path: &str) -> std::path::PathBuf {
    let path = Path::new(path);
    if super::settings::node_is_absolute(path) { path.to_path_buf() } else { cwd.join(path) }
}

/// The tool's file as a location, resolved against `cwd`.
pub fn tool_call_locations(args: &Value, cwd: &Path, line: Option<u32>) -> Option<Vec<ToolCallLocation>> {
    let path = tool_path(args)?;
    Some(vec![ToolCallLocation::new(resolve(cwd, path)).line(line)])
}

fn status_of(phase: ToolPhase) -> ToolCallStatus {
    match phase {
        ToolPhase::Pending => ToolCallStatus::Pending,
        ToolPhase::InProgress => ToolCallStatus::InProgress,
    }
}

fn text_content(text: String) -> Vec<ToolCallContent> {
    vec![ToolCallContent::from(ContentBlock::Text(TextContent::new(text)))]
}

/// The status text for `auto_retry_start`, with the delay rounded to whole seconds.
pub fn format_auto_retry_message(event: &Value) -> String {
    let number = |key: &str| event.get(key).and_then(Value::as_f64).filter(|value| value.is_finite());
    let (Some(round), Some(max), Some(delay)) = (number("attempt"), number("maxAttempts"), number("delayMs")) else {
        return "Retrying...".into();
    };
    let mut seconds = (delay / 1000.0).round() as i64;
    if delay > 0.0 && seconds == 0 {
        seconds = 1;
    }
    format!("Retrying (attempt {round}/{max}, waiting {seconds}s)...")
}

struct BashCall<'a> {
    id: &'a str,
    tool_name: &'a str,
    args: &'a Value,
    status: ToolCallStatus,
    locations: Option<Vec<ToolCallLocation>>,
    /// The first update for this call creates it, with the terminal content.
    create: bool,
}

impl AcpSession {
    pub async fn handle_pi_event(&self, event: Value) {
        match event["type"].as_str().unwrap_or_default() {
            "session_info_changed" => match event.get("name") {
                Some(Value::String(name)) => self.publish_title(Some(name.clone()), Some(iso_timestamp())),
                None => self.publish_title(None, Some(iso_timestamp())),
                Some(_) => {}
            },
            "message_update" => self.on_message_update(&event["assistantMessageEvent"]),
            "tool_execution_start" => self.on_tool_start(&event),
            "tool_execution_update" => self.on_tool_update(&event),
            "tool_execution_end" => self.on_tool_end(&event),
            "extension_ui_request" => {
                let session = self.this();
                tokio::spawn(async move { session.handle_extension_ui_request(event).await });
            }
            "auto_retry_start" => self.emit(text_chunk(format_auto_retry_message(&event))),
            "auto_retry_end" => self.emit(text_chunk("Retry finished, resuming.")),
            "auto_compaction_start" => self.emit(text_chunk(COMPACTION_START)),
            "auto_compaction_end" => self.emit(text_chunk(COMPACTION_END)),
            "compaction_start" if event["reason"] != "manual" => self.emit(text_chunk(COMPACTION_START)),
            "compaction_end"
                if event["reason"] != "manual" && event["aborted"] != true && event.get("errorMessage").is_none() =>
            {
                self.emit(text_chunk(COMPACTION_END))
            }
            "agent_start" => lock(&self.inner).in_agent_loop = true,
            "agent_end" => lock(&self.inner).in_agent_loop = false,
            "agent_settled" => self.settle_turn().await,
            _ => {}
        }
    }

    fn on_message_update(&self, event: &Value) {
        let kind = event["type"].as_str().unwrap_or_default();
        match (kind, event.get("delta").and_then(Value::as_str)) {
            ("text_delta", Some(delta)) => {
                let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(delta)));
                return self.emit(SessionUpdate::AgentMessageChunk(chunk));
            }
            ("thinking_delta", Some(delta)) => {
                let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(delta)));
                return self.emit(SessionUpdate::AgentThoughtChunk(chunk));
            }
            _ => {}
        }
        if !matches!(kind, "toolcall_start" | "toolcall_delta" | "toolcall_end") {
            return;
        }
        let index = event["contentIndex"].as_u64().unwrap_or(0) as usize;
        let call = event.get("toolCall").cloned().unwrap_or_else(|| event["partial"]["content"][index].clone());
        let id = match &call["id"] {
            Value::String(id) => id.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        };
        if id.is_empty() {
            return;
        }
        let name = call["name"].as_str().unwrap_or("tool").to_string();
        let raw_input = match call.get("arguments") {
            Some(arguments) if arguments.is_object() => Some(arguments.clone()),
            _ => call["partialArgs"]
                .as_str()
                .filter(|text| !text.is_empty())
                .map(|text| serde_json::from_str(text).unwrap_or_else(|_| json!({"partialArgs": text}))),
        };
        let args = raw_input.clone().unwrap_or(Value::Null);
        let locations = tool_call_locations(&args, &self.cwd, None);
        let existing = {
            let mut inner = lock(&self.inner);
            let existing = inner.tool_calls.get(&id).copied();
            if existing.is_none() {
                inner.tool_calls.insert(id.clone(), ToolPhase::Pending);
            }
            existing
        };
        let status = status_of(existing.unwrap_or(ToolPhase::Pending));
        if is_bash_tool(&name) {
            let create = existing.is_none();
            return self.emit_bash_call(BashCall { id: &id, tool_name: &name, args: &args, status, locations, create });
        }
        if existing.is_none() {
            let mut call = ToolCall::new(id, name.clone()).kind(tool_kind(&name)).status(status).raw_input(raw_input);
            if let Some(locations) = locations {
                call = call.locations(locations);
            }
            self.emit(SessionUpdate::ToolCall(call));
        } else {
            let fields = ToolCallUpdateFields::new().status(status).locations(locations).raw_input(raw_input);
            self.emit(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(id, fields)));
        }
    }

    fn emit_bash_call(&self, call: BashCall<'_>) {
        lock(&self.inner).bash_calls.insert(call.id.to_string());
        let title = bash_command(call.args).unwrap_or_else(|| call.tool_name.to_string());
        let cwd = self.cwd.display().to_string();
        if call.create {
            let mut tool = ToolCall::new(call.id.to_string(), title)
                .kind(ToolKind::Execute)
                .status(call.status)
                .content(bash_terminal_content(call.id))
                .meta(bash_terminal_info_meta(call.id, &cwd));
            if let Some(locations) = call.locations {
                tool = tool.locations(locations);
            }
            return self.emit(SessionUpdate::ToolCall(tool));
        }
        let fields = ToolCallUpdateFields::new()
            .title(title)
            .kind(ToolKind::Execute)
            .status(call.status)
            .locations(call.locations);
        self.emit(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(call.id.to_string(), fields)));
    }

    fn on_tool_start(&self, event: &Value) {
        let id = event["toolCallId"].as_str().map(String::from).unwrap_or_else(uuid_v4);
        let name = event["toolName"].as_str().unwrap_or("tool").to_string();
        let args = event.get("args").cloned().unwrap_or(Value::Null);
        if is_bash_tool(&name) {
            let existing = lock(&self.inner).tool_calls.insert(id.clone(), ToolPhase::InProgress);
            let locations = tool_call_locations(&args, &self.cwd, None);
            let status = ToolCallStatus::InProgress;
            let create = existing.is_none();
            return self.emit_bash_call(BashCall { id: &id, tool_name: &name, args: &args, status, locations, create });
        }
        let mut line = None;
        if name == "edit" || name == "write" {
            lock(&self.inner).mutations.insert(id.clone());
            if let Some(path) = tool_path(&args) {
                let old_text = match event.get("fileSnapshot") {
                    Some(snapshot) => snapshot["oldText"].as_str().map(String::from),
                    None => std::fs::read_to_string(resolve(&self.cwd, path)).ok(),
                };
                if name == "edit"
                    && let Some(text) = &old_text
                {
                    line = edit_old_texts(&args).iter().find_map(|needle| find_unique_line_number(text, needle));
                }
                let snapshot = Snapshot { path: path.to_string(), old_text };
                lock(&self.inner).snapshots.insert(id.clone(), snapshot);
            }
        }
        let locations = tool_call_locations(&args, &self.cwd, line);
        let existing = lock(&self.inner).tool_calls.insert(id.clone(), ToolPhase::InProgress);
        if existing.is_none() {
            let mut call = ToolCall::new(id, name.clone())
                .kind(tool_kind(&name))
                .status(ToolCallStatus::InProgress)
                .raw_input(args);
            if let Some(locations) = locations {
                call = call.locations(locations);
            }
            self.emit(SessionUpdate::ToolCall(call));
        } else {
            let fields =
                ToolCallUpdateFields::new().status(ToolCallStatus::InProgress).locations(locations).raw_input(args);
            self.emit(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(id, fields)));
        }
    }

    fn emit_bash_output(&self, id: &str, status: ToolCallStatus, result: &Value, is_error: bool) {
        let text = bash_result_text(result);
        let sent = lock(&self.inner).bash_output.insert(id.to_string(), text.clone()).unwrap_or_default();
        let delta = bash_output_delta(&sent, &text);
        let mut meta = if delta.is_empty() { Default::default() } else { bash_terminal_output_meta(id, delta) };
        if matches!(status, ToolCallStatus::Completed | ToolCallStatus::Failed) {
            meta.extend(bash_terminal_exit_meta(id, bash_exit_code(result, is_error)));
        }
        let update = ToolCallUpdate::new(id.to_string(), ToolCallUpdateFields::new().status(status)).meta(meta);
        self.emit(SessionUpdate::ToolCallUpdate(update));
    }

    fn on_tool_update(&self, event: &Value) {
        let Some(id) = event["toolCallId"].as_str().filter(|id| !id.is_empty()) else { return };
        let partial = event.get("partialResult").cloned().unwrap_or(Value::Null);
        let (bash, mutation) = {
            let inner = lock(&self.inner);
            (inner.bash_calls.contains(id), inner.mutations.contains(id))
        };
        if bash {
            return self.emit_bash_output(id, ToolCallStatus::InProgress, &partial, false);
        }
        let text = if mutation { String::new() } else { tool_result_to_text(&partial) };
        let mut fields = ToolCallUpdateFields::new().status(ToolCallStatus::InProgress);
        if !text.is_empty() {
            fields = fields.content(text_content(text));
        }
        if !mutation {
            fields = fields.raw_output(partial);
        }
        self.emit(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(id.to_string(), fields)));
    }

    fn on_tool_end(&self, event: &Value) {
        let Some(id) = event["toolCallId"].as_str().filter(|id| !id.is_empty()).map(String::from) else { return };
        let result = event.get("result").cloned().unwrap_or(Value::Null);
        let is_error = event["isError"].as_bool().unwrap_or(false);
        let status = if is_error { ToolCallStatus::Failed } else { ToolCallStatus::Completed };
        if lock(&self.inner).bash_calls.contains(&id) {
            self.emit_bash_output(&id, status, &result, is_error);
            return self.cleanup_tool_call(&id);
        }
        let text = tool_result_to_text(&result);
        let snapshot = lock(&self.inner).snapshots.remove(&id);
        let mut diff = None;
        if !is_error
            && let Some(snapshot) = snapshot
            && let Some(new_text) = match event.get("fileAfter") {
                Some(after) => after["newText"].as_str().map(String::from),
                None => std::fs::read_to_string(resolve(&self.cwd, &snapshot.path)).ok(),
            }
            && snapshot.old_text.as_deref() != Some(new_text.as_str())
        {
            diff = Some(Diff::new(snapshot.path, new_text).old_text(snapshot.old_text));
        }
        let mut fields = ToolCallUpdateFields::new().status(status);
        match diff {
            Some(diff) => fields = fields.content(vec![ToolCallContent::Diff(diff)]),
            None => {
                if !text.is_empty() {
                    fields = fields.content(text_content(text));
                }
                fields = fields.raw_output(result);
            }
        }
        self.emit(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(id.clone(), fields)));
        self.cleanup_tool_call(&id);
    }

    fn cleanup_tool_call(&self, id: &str) {
        let mut inner = lock(&self.inner);
        inner.tool_calls.remove(id);
        inner.snapshots.remove(id);
        inner.mutations.remove(id);
        inner.bash_calls.remove(id);
        inner.bash_output.remove(id);
    }
}
