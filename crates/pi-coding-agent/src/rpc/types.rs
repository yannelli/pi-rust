//! Protocol records (upstream `modes/rpc/rpc-types.ts`): commands read from stdin, responses,
//! session state, discoverable commands, and bash results, in upstream JSON shapes.

use pi_agent::QueueMode;
use pi_ai::{ContentBlock, Model, ThinkingLevel};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::agent_session::{BashResult, StreamingBehavior};
use crate::resources::SourceInfo;

/// Every command `type`, in the order of `rpc-commands.md`.
pub const COMMAND_TYPES: [&str; 34] = [
    "prompt",
    "steer",
    "follow_up",
    "abort",
    "clear_queue",
    "new_session",
    "get_state",
    "get_messages",
    "set_model",
    "cycle_model",
    "get_available_models",
    "set_thinking_level",
    "cycle_thinking_level",
    "get_available_thinking_levels",
    "set_fast_mode",
    "set_steering_mode",
    "set_follow_up_mode",
    "compact",
    "set_auto_compaction",
    "set_auto_retry",
    "abort_retry",
    "bash",
    "abort_bash",
    "get_session_stats",
    "export_html",
    "switch_session",
    "fork",
    "clone",
    "get_fork_messages",
    "get_entries",
    "get_tree",
    "get_last_assistant_text",
    "set_session_name",
    "get_commands",
];

/// How a prompt sent during a run is queued (`streamingBehavior`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RpcStreamingBehavior {
    Steer,
    FollowUp,
}

impl From<RpcStreamingBehavior> for StreamingBehavior {
    fn from(behavior: RpcStreamingBehavior) -> Self {
        match behavior {
            RpcStreamingBehavior::Steer => Self::Steer,
            RpcStreamingBehavior::FollowUp => Self::FollowUp,
        }
    }
}

/// One command without its `id` (upstream `RpcCommand`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum RpcCommand {
    Prompt {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        images: Option<Vec<ContentBlock>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        streaming_behavior: Option<RpcStreamingBehavior>,
    },
    Steer {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        images: Option<Vec<ContentBlock>>,
    },
    FollowUp {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        images: Option<Vec<ContentBlock>>,
    },
    Abort,
    ClearQueue,
    NewSession {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_session: Option<String>,
    },
    GetState,
    SetModel {
        provider: String,
        model_id: String,
    },
    CycleModel,
    GetAvailableModels,
    SetThinkingLevel {
        level: ThinkingLevel,
    },
    CycleThinkingLevel,
    GetAvailableThinkingLevels,
    /// pi-rust extension.
    SetFastMode {
        enabled: bool,
    },
    SetSteeringMode {
        mode: QueueMode,
    },
    SetFollowUpMode {
        mode: QueueMode,
    },
    Compact {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        custom_instructions: Option<String>,
    },
    SetAutoCompaction {
        enabled: bool,
    },
    SetAutoRetry {
        enabled: bool,
    },
    AbortRetry,
    Bash {
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exclude_from_context: Option<bool>,
    },
    AbortBash,
    GetSessionStats,
    ExportHtml {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_path: Option<String>,
    },
    SwitchSession {
        session_path: String,
    },
    Fork {
        entry_id: String,
    },
    #[serde(rename = "clone")]
    CloneSession,
    GetForkMessages,
    GetEntries {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        since: Option<String>,
    },
    GetTree,
    GetLastAssistantText,
    SetSessionName {
        name: String,
    },
    GetMessages,
    GetCommands,
}

/// One input line, routed by its `type`.
#[derive(Clone, Debug, PartialEq)]
pub enum InputRecord {
    /// An `extension_ui_response` for a pending dialog.
    UiResponse { id: String, response: Map<String, Value> },
    /// A command; `command` is the error response text when it cannot be handled.
    Command { id: Option<Value>, kind: Option<Value>, command: Result<RpcCommand, String> },
}

/// A `type` value as upstream prints it in `Unknown command: ...`.
fn type_label(kind: Option<&Value>) -> String {
    match kind {
        None | Some(Value::Null) => "undefined".into(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

/// Parse one line; `Err` holds the `parse` error text for malformed JSON.
pub fn parse_input(line: &str) -> Result<InputRecord, String> {
    let parsed: Value = serde_json::from_str(line).map_err(|error| format!("Failed to parse command: {error}"))?;
    let Value::Object(object) = parsed else {
        return Ok(InputRecord::Command { id: None, kind: None, command: Err("Unknown command: undefined".into()) });
    };
    let kind = object.get("type").cloned();
    if kind.as_ref().and_then(Value::as_str) == Some("extension_ui_response") {
        let id = match object.get("id") {
            Some(Value::String(id)) => id.clone(),
            Some(other) => other.to_string(),
            None => String::new(),
        };
        return Ok(InputRecord::UiResponse { id, response: object });
    }
    let id = object.get("id").filter(|id| !id.is_null()).cloned();
    let known = kind.as_ref().and_then(Value::as_str).is_some_and(|kind| COMMAND_TYPES.contains(&kind));
    let command = if known {
        serde_json::from_value(Value::Object(object)).map_err(|error| error.to_string())
    } else {
        Err(format!("Unknown command: {}", type_label(kind.as_ref())))
    };
    Ok(InputRecord::Command { id, kind, command })
}

/// A success response; `data` is left out when `None`.
pub fn success(id: Option<&Value>, command: &str, data: Option<Value>) -> Value {
    let mut response = Map::new();
    if let Some(id) = id {
        response.insert("id".into(), id.clone());
    }
    response.insert("type".into(), json!("response"));
    response.insert("command".into(), json!(command));
    response.insert("success".into(), json!(true));
    if let Some(data) = data {
        response.insert("data".into(), data);
    }
    Value::Object(response)
}

/// A failure response; `command` is left out when the record had no `type`.
pub fn error(id: Option<&Value>, command: Option<&Value>, message: &str) -> Value {
    let mut response = Map::new();
    if let Some(id) = id {
        response.insert("id".into(), id.clone());
    }
    response.insert("type".into(), json!("response"));
    if let Some(command) = command.filter(|command| !command.is_null()) {
        response.insert("command".into(), command.clone());
    }
    response.insert("success".into(), json!(false));
    response.insert("error".into(), json!(message));
    Value::Object(response)
}

/// A success response line with pre-serialized `data`, for payloads too deep to build as a
/// [`Value`] (the `get_tree` tree).
pub fn success_line_with_raw_data(id: Option<&Value>, command: &str, raw_data: &str) -> String {
    let envelope = success(id, command, Some(Value::Null)).to_string();
    let prefix = envelope.strip_suffix("null}").expect("data is the last field");
    format!("{prefix}{raw_data}}}\n")
}

/// `get_state` data (upstream `RpcSessionState`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcSessionState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Model>,
    pub thinking_level: ThinkingLevel,
    /// pi-rust extension.
    pub fast_mode: bool,
    /// pi-rust extension: whether the current model has a fast mode.
    pub fast_mode_available: bool,
    pub is_streaming: bool,
    pub is_compacting: bool,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    pub auto_compaction_enabled: bool,
    pub message_count: usize,
    pub pending_message_count: usize,
}

/// What registered a discoverable command.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RpcCommandSource {
    Extension,
    Prompt,
    Skill,
}

/// A command a client can run through `prompt` as `/name` (upstream `RpcSlashCommand`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcSlashCommand {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub source: RpcCommandSource,
    pub source_info: SourceInfo,
}

/// `bash` data (upstream `BashResult`); `exitCode` is absent for a cancelled command.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcBashResult {
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    pub cancelled: bool,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
}

impl From<&BashResult> for RpcBashResult {
    fn from(result: &BashResult) -> Self {
        Self {
            output: result.output.clone(),
            exit_code: result.exit_code.map(i64::from),
            cancelled: result.cancelled,
            truncated: result.truncated,
            full_output_path: result.full_output_path.as_ref().map(|path| path.display().to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands_ids_and_ui_responses() {
        let parsed = parse_input(r#"{"id":"a","type":"set_model","provider":"p","modelId":"m"}"#).unwrap();
        let InputRecord::Command { id, command, .. } = parsed else { panic!("command") };
        assert_eq!(id, Some(json!("a")));
        assert_eq!(command.unwrap(), RpcCommand::SetModel { provider: "p".into(), model_id: "m".into() });

        let parsed = parse_input(r#"{"type":"clone","id":7}"#).unwrap();
        assert!(matches!(parsed, InputRecord::Command { command: Ok(RpcCommand::CloneSession), .. }));
        let parsed = parse_input(r#"{"type":"prompt","message":"hi","streamingBehavior":"followUp","images":null}"#);
        let InputRecord::Command { command: Ok(RpcCommand::Prompt { streaming_behavior, images, .. }), .. } =
            parsed.unwrap()
        else {
            panic!("prompt")
        };
        assert_eq!((streaming_behavior, images), (Some(RpcStreamingBehavior::FollowUp), None));

        let parsed = parse_input(r#"{"type":"extension_ui_response","id":"u1","value":"Allow"}"#).unwrap();
        let InputRecord::UiResponse { id, response } = parsed else { panic!("ui response") };
        assert_eq!((id.as_str(), &response["value"]), ("u1", &json!("Allow")));
    }

    #[test]
    fn reports_unknown_and_malformed_commands() {
        let parsed = parse_input(r#"{"id":"x","type":"nope"}"#).unwrap();
        let InputRecord::Command { command: Err(message), kind, .. } = parsed else { panic!("error") };
        assert_eq!((message.as_str(), kind), ("Unknown command: nope", Some(json!("nope"))));
        let parsed = parse_input("[1]").unwrap();
        assert!(
            matches!(parsed, InputRecord::Command { command: Err(message), .. } if message == "Unknown command: undefined")
        );
        let parsed = parse_input(r#"{"type":"set_model"}"#).unwrap();
        assert!(matches!(parsed, InputRecord::Command { command: Err(message), .. } if message.contains("provider")));
        assert!(parse_input("not json").unwrap_err().starts_with("Failed to parse command: "));
        assert!(parse_input("").unwrap_err().starts_with("Failed to parse command: "));
    }

    #[test]
    fn builds_responses_in_upstream_field_order() {
        let ok = success(Some(&json!("r1")), "get_tree", Some(json!({"a": 1})));
        assert_eq!(
            ok.to_string(),
            r#"{"id":"r1","type":"response","command":"get_tree","success":true,"data":{"a":1}}"#
        );
        assert_eq!(
            success_line_with_raw_data(None, "get_tree", r#"{"tree":[]}"#),
            "{\"type\":\"response\",\"command\":\"get_tree\",\"success\":true,\"data\":{\"tree\":[]}}\n"
        );
        let failed = error(None, Some(&json!("parse")), "boom");
        assert_eq!(failed.to_string(), r#"{"type":"response","command":"parse","success":false,"error":"boom"}"#);
        assert_eq!(COMMAND_TYPES.len(), 34);
    }
}
