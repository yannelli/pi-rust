use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{
    ContentBlock, IntoOption, McpServer, Meta, SessionConfigId, SessionConfigOption, SessionConfigValueId, SessionId,
    SessionModeId,
};

/// `session/new` params.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionRequest {
    pub cwd: PathBuf,
    #[serde(default)]
    pub additional_directories: Vec<PathBuf>,
    pub mcp_servers: Vec<McpServer>,
    #[serde(rename = "_meta")]
    pub meta: Option<Meta>,
}

/// `session/load` params.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadSessionRequest {
    pub mcp_servers: Vec<McpServer>,
    pub cwd: PathBuf,
    #[serde(default)]
    pub additional_directories: Vec<PathBuf>,
    pub session_id: SessionId,
    #[serde(rename = "_meta")]
    pub meta: Option<Meta>,
}

/// `session/list` params.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct ListSessionsRequest {
    pub cwd: Option<PathBuf>,
    pub cursor: Option<String>,
    #[serde(rename = "_meta")]
    pub meta: Option<Meta>,
}

/// `session/list` result.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListSessionsResponse {
    pub sessions: Vec<SessionInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

impl ListSessionsResponse {
    pub fn new(sessions: Vec<SessionInfo>) -> Self {
        Self { sessions, next_cursor: None, meta: None }
    }

    pub fn next_cursor(mut self, next_cursor: impl IntoOption<String>) -> Self {
        self.next_cursor = next_cursor.into_option();
        self
    }

    pub fn meta(mut self, meta: impl IntoOption<Meta>) -> Self {
        self.meta = meta.into_option();
        self
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub session_id: SessionId,
    pub cwd: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

impl SessionInfo {
    pub fn new(session_id: impl Into<SessionId>, cwd: impl Into<PathBuf>) -> Self {
        Self { session_id: session_id.into(), cwd: cwd.into(), title: None, updated_at: None, meta: None }
    }

    pub fn title(mut self, title: impl IntoOption<String>) -> Self {
        self.title = title.into_option();
        self
    }

    pub fn updated_at(mut self, updated_at: impl IntoOption<String>) -> Self {
        self.updated_at = updated_at.into_option();
        self
    }
}

/// Params that name only a session: `session/delete`, `session/close`, and `session/cancel`.
macro_rules! session_params {
    ($($name:ident),* $(,)?) => {$(
        #[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
        #[serde(rename_all = "camelCase")]
        pub struct $name {
            pub session_id: SessionId,
            #[serde(rename = "_meta")]
            pub meta: Option<Meta>,
        }
    )*};
}

session_params!(DeleteSessionRequest, CloseSessionRequest, CancelNotification);

/// `session/prompt` params.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptRequest {
    pub session_id: SessionId,
    pub prompt: Vec<ContentBlock>,
    #[serde(rename = "_meta")]
    pub meta: Option<Meta>,
}

/// `session/prompt` result, with the unstable end-of-turn `usage`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptResponse {
    pub stop_reason: StopReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

impl PromptResponse {
    pub fn new(stop_reason: StopReason) -> Self {
        Self { stop_reason, usage: None, meta: None }
    }

    pub fn usage(mut self, usage: impl IntoOption<Usage>) -> Self {
        self.usage = usage.into_option();
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
}

/// Token counts of a turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub total_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_read_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_write_tokens: Option<u64>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

impl Usage {
    pub fn new(total_tokens: u64, input_tokens: u64, output_tokens: u64) -> Self {
        Self {
            total_tokens,
            input_tokens,
            output_tokens,
            thought_tokens: None,
            cached_read_tokens: None,
            cached_write_tokens: None,
            meta: None,
        }
    }
}

/// Legacy `session/set_mode` params.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionModeRequest {
    pub session_id: SessionId,
    pub mode_id: SessionModeId,
    #[serde(rename = "_meta")]
    pub meta: Option<Meta>,
}

/// `session/set_config_option` params; `value` is a string (boolean options are off).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionConfigOptionRequest {
    pub session_id: SessionId,
    pub config_id: SessionConfigId,
    pub value: SessionConfigValueId,
    #[serde(rename = "_meta")]
    pub meta: Option<Meta>,
}

/// `session/set_config_option` result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionConfigOptionResponse {
    pub config_options: Vec<SessionConfigOption>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

impl SetSessionConfigOptionResponse {
    pub fn new(config_options: Vec<SessionConfigOption>) -> Self {
        Self { config_options, meta: None }
    }
}
