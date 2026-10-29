//! ACP tool approval policy. These modes are not an OS sandbox.

use std::sync::{Arc, Mutex};

use agent_client_protocol_schema::{
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest, SessionMode,
    SessionModeState, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use pi_ai::ToolCall;
use tokio_util::sync::CancellationToken;

use super::backend::AcpClient;
use crate::agent_session::lock;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PermissionMode {
    #[default]
    Auto,
    Ask,
    Bypass,
}

impl PermissionMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "ask" => Some(Self::Ask),
            "bypass" => Some(Self::Bypass),
            _ => None,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Ask => "ask",
            Self::Bypass => "bypass",
        }
    }

    pub fn state(self) -> SessionModeState {
        SessionModeState::new(
            self.id(),
            vec![
                SessionMode::new("auto", "Auto").description("Approve read-only tools; ask before other tools"),
                SessionMode::new("ask", "Ask").description("Ask before every tool call"),
                SessionMode::new("bypass", "Bypass").description("Run tools without ACP approval prompts"),
            ],
        )
    }
}

#[derive(Default)]
pub struct ToolPermissions {
    mode: Mutex<PermissionMode>,
    client: Mutex<Option<(String, Arc<dyn AcpClient>)>>,
}

impl ToolPermissions {
    pub fn bind(&self, session_id: &str, client: Arc<dyn AcpClient>) {
        *lock(&self.client) = Some((session_id.to_string(), client));
    }

    pub fn mode(&self) -> PermissionMode {
        *lock(&self.mode)
    }

    pub fn set_mode(&self, mode: PermissionMode) {
        *lock(&self.mode) = mode;
    }

    pub async fn authorize(
        &self,
        call: ToolCall,
        args: serde_json::Value,
        read_only: bool,
        cancel: CancellationToken,
    ) -> bool {
        if cancel.is_cancelled() {
            return false;
        }
        match self.mode() {
            PermissionMode::Bypass => return true,
            PermissionMode::Auto if read_only => return true,
            _ => {}
        }
        let Some((session_id, client)) = lock(&self.client).clone() else { return false };
        let fields = ToolCallUpdateFields::new()
            .title(call.name.clone())
            .kind(super::translate::tool_kind(&call.name))
            .status(ToolCallStatus::Pending)
            .raw_input(args);
        let request = RequestPermissionRequest::new(
            session_id,
            ToolCallUpdate::new(call.id, fields),
            vec![
                PermissionOption::new("allow", "Allow once", PermissionOptionKind::AllowOnce),
                PermissionOption::new("reject", "Reject", PermissionOptionKind::RejectOnce),
            ],
        );
        tokio::select! {
            biased;
            _ = cancel.cancelled() => false,
            response = client.request_permission(request) => matches!(
                response.map(|response| response.outcome),
                Ok(RequestPermissionOutcome::Selected(selected)) if selected.option_id.to_string() == "allow"
            ),
        }
    }
}
