//! Shared helpers for the server test suites.
#![allow(dead_code)]

use std::time::Duration;

use pi_chord::Json;
use pi_chord::services::ServiceCall;
use pi_protocol::{ResponseEnvelope, ServerMessage};
use pi_server::testing::ProtocolTestClient;

pub const SERVER_ID: &str = "00000000-0000-4000-8000-000000000001";

pub fn session_call(member: &str, args: Vec<Json>) -> ServiceCall {
    ServiceCall { service_id: "test.session".into(), instance: None, member: member.into(), args }
}

pub fn call(service_id: &str, member: &str) -> ServiceCall {
    ServiceCall { service_id: service_id.into(), instance: None, member: member.into(), args: vec![] }
}

/// Wait until `condition` holds, failing after five seconds.
pub async fn poll_until(mut condition: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(tokio::time::Instant::now() < deadline, "condition was not met in time");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

pub fn error_code(response: &ResponseEnvelope) -> Option<&str> {
    response.result.as_ref().err().map(|error| error.code.as_str())
}

pub fn assert_ok(response: &ResponseEnvelope) {
    assert!(response.result.is_ok(), "expected success, got {response:?}");
}

pub fn assert_error(response: &ResponseEnvelope, code: &str) {
    assert_eq!(error_code(response), Some(code), "{response:?}");
}

pub fn hello_error_code(message: &ServerMessage) -> Option<&str> {
    match message {
        ServerMessage::HelloError(error) => Some(error.error.code.as_str()),
        _ => None,
    }
}

pub fn latest_attachment_id(client: &ProtocolTestClient, session_id: &str) -> String {
    client
        .messages()
        .iter()
        .rev()
        .find_map(|message| match message {
            ServerMessage::Attachment(envelope) => {
                envelope.attachment.as_ref().filter(|target| target.session_id == session_id)
            }
            _ => None,
        })
        .map(|target| target.attachment_id.clone())
        .unwrap_or_else(|| panic!("Missing attachment for {session_id}"))
}
