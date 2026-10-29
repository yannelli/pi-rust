use std::sync::Arc;

use agent_client_protocol_schema::{
    Error, RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome,
    SessionNotification,
};
use async_trait::async_trait;
use pi_ai::ToolCall;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::FakeClient;
use crate::acp::backend::AcpClient;
use crate::acp::permissions::{PermissionMode, ToolPermissions};

fn call() -> ToolCall {
    ToolCall {
        id: "call-1".into(),
        name: "mcp__docs__search".into(),
        arguments: Default::default(),
        ..Default::default()
    }
}

#[tokio::test]
async fn modes_gate_calls_and_unknown_or_failed_answers_deny() {
    let client = FakeClient::new();
    let policy = ToolPermissions::default();
    policy.bind("s1", client.clone());
    for (mode, read_only, prompts) in [
        (PermissionMode::Auto, true, false),
        (PermissionMode::Auto, false, true),
        (PermissionMode::Ask, true, true),
        (PermissionMode::Ask, false, true),
        (PermissionMode::Bypass, false, false),
    ] {
        policy.set_mode(mode);
        client.permission_requests.lock().unwrap().clear();
        assert!(policy.authorize(call(), json!({"query": "updated args"}), read_only, CancellationToken::new()).await);
        let requests = client.permission_requests.lock().unwrap();
        assert_eq!(!requests.is_empty(), prompts);
        if let Some(request) = requests.first() {
            let value = serde_json::to_value(request).unwrap();
            assert_eq!(value["sessionId"], "s1");
            assert_eq!(value["toolCall"]["toolCallId"], "call-1");
            assert_eq!(value["toolCall"]["title"], "mcp__docs__search");
            assert_eq!(value["toolCall"]["rawInput"], json!({"query": "updated args"}));
        }
    }
    policy.set_mode(PermissionMode::Ask);
    for selected in ["reject", "unknown", "allow-always"] {
        client.answer(Ok(RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new(selected),
        ))));
        assert!(!policy.authorize(call(), json!({}), false, CancellationToken::new()).await);
    }
    client.answer(Err(Error::internal_error()));
    assert!(!policy.authorize(call(), json!({}), false, CancellationToken::new()).await);
    assert!(!ToolPermissions::default().authorize(call(), json!({}), false, CancellationToken::new()).await);
}

struct WaitingClient(tokio::sync::Notify);

#[async_trait]
impl AcpClient for WaitingClient {
    fn session_update(&self, _: SessionNotification) {}

    async fn request_permission(&self, _: RequestPermissionRequest) -> Result<RequestPermissionResponse, Error> {
        self.0.notify_one();
        std::future::pending().await
    }
}

#[tokio::test]
async fn cancellation_interrupts_approval_and_also_denies_bypass_calls() {
    let policy = Arc::new(ToolPermissions::default());
    let client = Arc::new(WaitingClient(tokio::sync::Notify::new()));
    policy.bind("s1", client.clone());
    let cancel = CancellationToken::new();
    let pending = {
        let (policy, cancel) = (policy.clone(), cancel.clone());
        tokio::spawn(async move { policy.authorize(call(), json!({}), false, cancel).await })
    };
    client.0.notified().await;
    cancel.cancel();
    assert!(!tokio::time::timeout(std::time::Duration::from_secs(1), pending).await.unwrap().unwrap());
    policy.set_mode(PermissionMode::Bypass);
    assert!(!policy.authorize(call(), json!({}), false, cancel).await);
}
