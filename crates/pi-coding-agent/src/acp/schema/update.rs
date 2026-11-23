use serde::Serialize;

use super::{
    ContentBlock, IntoMaybeUndefined, IntoOption, MaybeUndefined, MessageId, Meta, SessionConfigOption, SessionId,
    SessionModeId, ToolCall, ToolCallUpdate,
};

/// `session/update` params.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNotification {
    pub session_id: SessionId,
    pub update: SessionUpdate,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

impl SessionNotification {
    pub fn new(session_id: impl Into<SessionId>, update: SessionUpdate) -> Self {
        Self { session_id: session_id.into(), update, meta: None }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
pub enum SessionUpdate {
    UserMessageChunk(ContentChunk),
    AgentMessageChunk(ContentChunk),
    AgentThoughtChunk(ContentChunk),
    ToolCall(ToolCall),
    ToolCallUpdate(ToolCallUpdate),
    AvailableCommandsUpdate(AvailableCommandsUpdate),
    CurrentModeUpdate(CurrentModeUpdate),
    ConfigOptionUpdate(ConfigOptionUpdate),
    SessionInfoUpdate(SessionInfoUpdate),
    UsageUpdate(UsageUpdate),
}

/// A piece of a message; chunks with the same `messageId` belong to one message.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentChunk {
    pub content: ContentBlock,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<MessageId>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

impl ContentChunk {
    pub fn new(content: ContentBlock) -> Self {
        Self { content, message_id: None, meta: None }
    }

    pub fn message_id(mut self, message_id: impl IntoOption<MessageId>) -> Self {
        self.message_id = message_id.into_option();
        self
    }

    pub fn meta(mut self, meta: impl IntoOption<Meta>) -> Self {
        self.meta = meta.into_option();
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentModeUpdate {
    pub current_mode_id: SessionModeId,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

impl CurrentModeUpdate {
    pub fn new(current_mode_id: impl Into<SessionModeId>) -> Self {
        Self { current_mode_id: current_mode_id.into(), meta: None }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOptionUpdate {
    pub config_options: Vec<SessionConfigOption>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

impl ConfigOptionUpdate {
    pub fn new(config_options: Vec<SessionConfigOption>) -> Self {
        Self { config_options, meta: None }
    }
}

/// Session metadata changes; a `null` title clears it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfoUpdate {
    #[serde(skip_serializing_if = "MaybeUndefined::is_undefined")]
    pub title: MaybeUndefined<String>,
    #[serde(skip_serializing_if = "MaybeUndefined::is_undefined")]
    pub updated_at: MaybeUndefined<String>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

impl SessionInfoUpdate {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn title(mut self, title: impl IntoMaybeUndefined<String>) -> Self {
        self.title = title.into_maybe_undefined();
        self
    }

    pub fn updated_at(mut self, updated_at: impl IntoMaybeUndefined<String>) -> Self {
        self.updated_at = updated_at.into_maybe_undefined();
        self
    }

    pub fn meta(mut self, meta: impl IntoOption<Meta>) -> Self {
        self.meta = meta.into_option();
        self
    }
}

/// Context window occupancy.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UsageUpdate {
    pub used: u64,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

impl UsageUpdate {
    pub fn new(used: u64, size: u64) -> Self {
        Self { used, size, cost: None, meta: None }
    }

    pub fn cost(mut self, cost: impl IntoOption<Cost>) -> Self {
        self.cost = cost.into_option();
        self
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Cost {
    pub amount: f64,
    pub currency: String,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

impl Cost {
    pub fn new(amount: f64, currency: impl Into<String>) -> Self {
        Self { amount, currency: currency.into(), meta: None }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableCommandsUpdate {
    pub available_commands: Vec<AvailableCommand>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

impl AvailableCommandsUpdate {
    pub fn new(available_commands: Vec<AvailableCommand>) -> Self {
        Self { available_commands, meta: None }
    }
}

/// A slash command the client can offer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AvailableCommand {
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<AvailableCommandInput>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

impl AvailableCommand {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self { name: name.into(), description: description.into(), input: None, meta: None }
    }

    pub fn input(mut self, input: impl IntoOption<AvailableCommandInput>) -> Self {
        self.input = input.into_option();
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum AvailableCommandInput {
    Unstructured(UnstructuredCommandInput),
}

/// Free text after the command, described by `hint`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UnstructuredCommandInput {
    pub hint: String,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

impl UnstructuredCommandInput {
    pub fn new(hint: impl Into<String>) -> Self {
        Self { hint: hint.into(), meta: None }
    }
}
