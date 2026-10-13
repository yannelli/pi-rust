//! Shared helpers for the `openai-completions` tests.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::OpenAICompletions;
use crate::models::builtin_models;
use crate::providers::mock_server::frame;
use crate::registry::{ApiProvider, StreamOptions};
use crate::types::{AssistantMessage, ContentBlock, Context, Message, Model, StopReason, Tool, ToolCall, Usage};

pub(super) const API: &str = "openai-completions";

pub(super) fn chunk(delta: Value) -> String {
    frame(None, &json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": delta}]}))
}

pub(super) fn finish(reason: &str) -> String {
    frame(None, &json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {}, "finish_reason": reason}]}))
}

pub(super) fn done() -> String {
    "data: [DONE]\n\n".to_string()
}

/// A complete response with a stop finish reason.
pub(super) fn ok_frames() -> Vec<String> {
    vec![chunk(json!({"content": "ok"})), finish("stop"), done()]
}

pub(super) fn options() -> StreamOptions {
    StreamOptions { api_key: Some("test-key".into()), ..StreamOptions::default() }
}

pub(super) fn user_context(text: &str) -> Context {
    Context { messages: vec![Message::user(text)], ..Context::default() }
}

pub(super) fn tool(name: &str) -> Tool {
    Tool::new(name, format!("{name} tool"), json!({"type": "object", "properties": {}}))
}

pub(super) fn system(value: Value) -> Message {
    serde_json::from_value(value).unwrap()
}

/// A bundled catalog model.
pub(super) fn catalog(provider: &str, id: &str) -> Model {
    builtin_models()
        .iter()
        .find(|model| model.provider == provider && model.id == id)
        .cloned()
        .unwrap_or_else(|| panic!("catalog model {provider}/{id}"))
}

/// A catalog model served through Chat Completions without its compat settings.
pub(super) fn plain_catalog(provider: &str, id: &str) -> Model {
    Model { api: API.into(), compat: None, ..catalog(provider, id) }
}

/// The request body the provider would send. The payload hook cancels the request, so
/// nothing leaves the process even for real provider URLs.
pub(super) async fn capture(model: &Model, context: &Context, mut options: StreamOptions) -> Value {
    let captured: Arc<Mutex<Option<Value>>> = Arc::default();
    let slot = captured.clone();
    let cancel = options.cancel.clone();
    options.on_payload = Some(crate::registry::payload_hook(move |payload, _| {
        *slot.lock().unwrap() = Some(payload.clone());
        cancel.cancel();
        None
    }));
    OpenAICompletions.stream(model, context, options).result(model).await;
    captured.lock().unwrap().take().expect("payload captured")
}

pub(super) fn tool_call(id: &str, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: arguments.as_object().cloned().unwrap(),
        ..Default::default()
    }
}

pub(super) fn assistant(model: &Model, content: Vec<ContentBlock>, stop_reason: StopReason) -> Message {
    Message::Assistant(AssistantMessage {
        content,
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        usage: Usage::default(),
        stop_reason,
        timestamp: 2,
        ..Default::default()
    })
}

pub(super) fn tool_result(id: &str, name: &str, content: Vec<ContentBlock>) -> Message {
    Message::ToolResult(crate::types::ToolResultMessage {
        tool_call_id: id.into(),
        tool_name: name.into(),
        content,
        is_error: false,
        timestamp: 3,
        ..Default::default()
    })
}

/// Messages of a body with the given role.
pub(super) fn with_role<'a>(body: &'a Value, role: &str) -> Vec<&'a Value> {
    body["messages"].as_array().unwrap().iter().filter(|message| message["role"] == role).collect()
}
