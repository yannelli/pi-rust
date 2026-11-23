use serde::{Deserialize, Serialize};

use super::{Meta, PermissionOptionId, SessionId, ToolCallUpdate};

/// `session/request_permission` params.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionRequest {
    pub session_id: SessionId,
    pub tool_call: ToolCallUpdate,
    pub options: Vec<PermissionOption>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

impl RequestPermissionRequest {
    pub fn new(session_id: impl Into<SessionId>, tool_call: ToolCallUpdate, options: Vec<PermissionOption>) -> Self {
        Self { session_id: session_id.into(), tool_call, options, meta: None }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    pub option_id: PermissionOptionId,
    pub name: String,
    pub kind: PermissionOptionKind,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

impl PermissionOption {
    pub fn new(option_id: impl Into<PermissionOptionId>, name: impl Into<String>, kind: PermissionOptionKind) -> Self {
        Self { option_id: option_id.into(), name: name.into(), kind, meta: None }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

/// `session/request_permission` result.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct RequestPermissionResponse {
    pub outcome: RequestPermissionOutcome,
    #[serde(rename = "_meta")]
    pub meta: Option<Meta>,
}

impl RequestPermissionResponse {
    pub fn new(outcome: RequestPermissionOutcome) -> Self {
        Self { outcome, meta: None }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RequestPermissionOutcome {
    Cancelled,
    Selected(SelectedPermissionOutcome),
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectedPermissionOutcome {
    pub option_id: PermissionOptionId,
    #[serde(rename = "_meta")]
    pub meta: Option<Meta>,
}

impl SelectedPermissionOutcome {
    pub fn new(option_id: impl Into<PermissionOptionId>) -> Self {
        Self { option_id: option_id.into(), meta: None }
    }
}
