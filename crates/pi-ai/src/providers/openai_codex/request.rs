//! Codex request pieces: URLs, ChatGPT account headers, the request body, and zstd bodies.

use std::collections::BTreeMap;

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use serde_json::{Map, Value, json};

use crate::identity::identity_headers;
use crate::provider_options::ReasoningSummary;
use crate::providers::common::compat_bool;
use crate::providers::constrained_sampling::{ResponsesToolOptions, convert_responses_tools};
use crate::providers::openai_responses::{ConvertOptions, convert_messages, responses_tool_support};
use crate::registry::{CacheRetention, StreamOptions};
use crate::simple_options::{clamp_openai_prompt_cache_key, clamp_thinking_level};
use crate::transcript::{initial_system_message, resolve_transcript_tools, system_message_text};
use crate::types::{Message, Model, ThinkingLevel};

pub(crate) const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api";
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";
pub(super) const OPENAI_BETA_RESPONSES_WEBSOCKETS: &str = "responses_websockets=2026-02-06";

/// `{base}/codex/responses`, accepting bases that already end in `/codex` or the full path.
pub(super) fn resolve_codex_url(base_url: &str) -> String {
    let raw = if base_url.trim().is_empty() { DEFAULT_CODEX_BASE_URL } else { base_url };
    let normalized = raw.trim_end_matches('/');
    if normalized.ends_with("/codex/responses") {
        normalized.to_string()
    } else if normalized.ends_with("/codex") {
        format!("{normalized}/responses")
    } else {
        format!("{normalized}/codex/responses")
    }
}

pub(super) fn resolve_codex_websocket_url(base_url: &str) -> String {
    let url = resolve_codex_url(base_url);
    match url.split_once("://") {
        Some(("https", rest)) => format!("wss://{rest}"),
        Some(("http", rest)) => format!("ws://{rest}"),
        _ => url,
    }
}

/// The ChatGPT account id claim of an OAuth access token.
pub(super) fn extract_account_id(token: &str) -> Result<String, String> {
    let failure = || "Failed to extract accountId from token".to_string();
    let parts: Vec<&str> = token.split('.').collect();
    let [_, payload, _] = parts.as_slice() else { return Err(failure()) };
    let trimmed = payload.trim_end_matches('=');
    let bytes = URL_SAFE_NO_PAD
        .decode(trimmed)
        .or_else(|_| STANDARD.decode(format!("{trimmed}{}", "=".repeat((4 - trimmed.len() % 4) % 4))))
        .map_err(|_| failure())?;
    let claims: Value = serde_json::from_slice(&bytes).map_err(|_| failure())?;
    claims[JWT_CLAIM_PATH]["chatgpt_account_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .ok_or_else(failure)
}

/// An ordered header list with case-insensitive replacement, like a fetch `Headers` object.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct HeaderList(pub Vec<(String, String)>);

impl HeaderList {
    pub fn set(&mut self, name: &str, value: impl Into<String>) {
        self.delete(name);
        self.0.push((name.to_string(), value.into()));
    }

    pub fn delete(&mut self, name: &str) {
        self.0.retain(|(key, _)| !key.eq_ignore_ascii_case(name));
    }
}

fn base_headers(model: &Model, options: &StreamOptions, account_id: &str, token: &str) -> HeaderList {
    let mut headers = HeaderList::default();
    // Codex expects an originator on every request; it is not attribution.
    headers.set("originator", "pi");
    for (name, value) in identity_headers(model, options.identity.as_ref()) {
        headers.set(name, value);
    }
    for (name, value) in model.headers.iter().chain(options.headers.iter()) {
        headers.set(name, value.clone());
    }
    for name in &options.suppressed_headers {
        headers.delete(name);
    }
    headers.set("Authorization", format!("Bearer {token}"));
    headers.set("chatgpt-account-id", account_id);
    headers
}

pub(super) fn sse_headers(
    model: &Model,
    options: &StreamOptions,
    account_id: &str,
    token: &str,
    session_id: Option<&str>,
) -> HeaderList {
    let mut headers = base_headers(model, options, account_id, token);
    headers.set("OpenAI-Beta", "responses=experimental");
    headers.set("accept", "text/event-stream");
    headers.set("content-type", "application/json");
    if let Some(session) = session_id {
        headers.set("session-id", session);
        headers.set("x-client-request-id", session);
    }
    headers
}

pub(super) fn websocket_headers(
    model: &Model,
    options: &StreamOptions,
    account_id: &str,
    token: &str,
    request_id: &str,
) -> HeaderList {
    let mut headers = base_headers(model, options, account_id, token);
    headers.delete("accept");
    headers.delete("content-type");
    headers.set("OpenAI-Beta", OPENAI_BETA_RESPONSES_WEBSOCKETS);
    headers.set("x-client-request-id", request_id);
    headers.set("session-id", request_id);
    headers
}

/// The session id used for prompt caching, clamped to OpenAI's 64-character limit.
pub(super) fn cache_session_id(options: &StreamOptions) -> Option<String> {
    if options.cache_retention == Some(CacheRetention::None) {
        return None;
    }
    options.session_id.as_deref().map(clamp_openai_prompt_cache_key)
}

/// Codex compat flags that shape the input and tools.
#[derive(Clone, Copy, Debug)]
pub(super) struct Compat {
    pub supports_strict_mode: bool,
    pub supports_grammar_tools: bool,
    pub supports_additional_tools: bool,
    pub supports_tool_search: bool,
    pub supports_mid_convo_system_messages: bool,
}

impl Compat {
    pub fn resolve(model: &Model) -> Self {
        let flag = |key: &str, default: bool| compat_bool(model, key).unwrap_or(default);
        Self {
            supports_strict_mode: flag("supportsStrictMode", true),
            supports_grammar_tools: flag("supportsOpenAIGrammarTools", false),
            supports_additional_tools: flag("supportsAdditionalTools", false),
            supports_tool_search: flag("supportsToolSearch", false),
            supports_mid_convo_system_messages: flag("supportsMidConvoSystemMessages", false),
        }
    }

    fn tool_options(self) -> ResponsesToolOptions {
        ResponsesToolOptions {
            strict: None,
            supports_strict_mode: self.supports_strict_mode,
            supports_openai_grammar_tools: self.supports_grammar_tools,
            tool_search_result: false,
        }
    }
}

/// Responses input items for a resolved transcript, without the leading system prompt.
pub(super) fn codex_input(
    model: &Model,
    messages: &[Message],
    compat: Compat,
    grammar: &BTreeMap<String, String>,
) -> Result<Vec<Value>, String> {
    let options = ConvertOptions {
        include_system_prompt: false,
        supports_mid_convo_system_messages: compat.supports_mid_convo_system_messages,
        supports_additional_tools: compat.supports_additional_tools,
        supports_tool_search: compat.supports_tool_search,
        grammar_tool_input_properties: grammar.clone(),
        tool_options: compat.tool_options(),
        ..ConvertOptions::default()
    };
    convert_messages(model, messages, &options)
}

/// The Codex body for a resolved transcript (leading system message first).
pub(super) fn build_body(
    model: &Model,
    messages: &[Message],
    options: &StreamOptions,
    cache_session_id: Option<&str>,
    grammar: &BTreeMap<String, String>,
) -> Result<Value, String> {
    let compat = Compat::resolve(model);
    let mut body = Map::new();
    body.insert("model".into(), json!(model.id));
    body.insert("store".into(), json!(false));
    body.insert("stream".into(), json!(true));
    let instructions = initial_system_message(messages).map(system_message_text).filter(|text| !text.is_empty());
    body.insert("instructions".into(), json!(instructions.as_deref().unwrap_or("You are a helpful assistant.")));
    body.insert("input".into(), Value::Array(codex_input(model, messages, compat, grammar)?));
    body.insert("text".into(), json!({"verbosity": "low"}));
    body.insert("include".into(), json!(["reasoning.encrypted_content"]));
    if let Some(session) = cache_session_id {
        body.insert("prompt_cache_key".into(), json!(session));
    }
    body.insert("tool_choice".into(), json!(options.tool_choice.as_deref().unwrap_or("auto")));
    body.insert("parallel_tool_calls".into(), json!(true));
    if let Some(temperature) = options.temperature {
        body.insert("temperature".into(), json!(temperature));
    }
    let support = responses_tool_support(
        compat.supports_mid_convo_system_messages,
        compat.supports_additional_tools,
        compat.supports_tool_search,
    );
    let tools = resolve_transcript_tools(messages, support).request_tools;
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(convert_responses_tools(&tools, compat.tool_options())?));
    }
    let map = model.thinking_level_map.as_ref();
    let effort =
        options.reasoning.map(|level| clamp_thinking_level(model, level)).filter(|level| *level != ThinkingLevel::Off);
    if let Some(level) = effort {
        let summary = options.reasoning_summary.map_or("auto", ReasoningSummary::as_str);
        let mapped = map.and_then(|map| map.get(level.as_str()).cloned().flatten());
        body.insert(
            "reasoning".into(),
            json!({"effort": mapped.as_deref().unwrap_or(level.as_str()), "summary": summary}),
        );
    } else if model.reasoning && map.and_then(|map| map.get("off")) != Some(&None) {
        let off = map.and_then(|map| map.get("off").cloned().flatten());
        body.insert("reasoning".into(), json!({"effort": off.as_deref().unwrap_or("none")}));
    }
    Ok(Value::Object(body))
}

/// Codex accepts zstd request bodies on the SSE endpoint, as the official client sends.
pub(super) fn compress_zstd(body: &[u8]) -> Vec<u8> {
    ruzstd::encoding::compress_to_vec(body, ruzstd::encoding::CompressionLevel::Fastest)
}

/// A random version 7 UUID for one-shot WebSocket request ids.
pub(super) fn uuid_v7() -> String {
    let mut bytes = [0u8; 16];
    let _ = ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes);
    let millis = crate::types::now_ms().max(0) as u64;
    bytes[..6].copy_from_slice(&millis.to_be_bytes()[2..]);
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = crate::providers::aws::sigv4::hex(&bytes);
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

#[cfg(test)]
mod summary_tests {
    use super::*;
    use crate::provider_options::ReasoningSummary;
    use crate::providers::mock_server::test_model;

    #[test]
    fn reasoning_summary_applies_only_with_an_effort() {
        let mut model = test_model("openai-codex-responses", "openai-codex", "https://chatgpt.com/backend-api");
        model.reasoning = true;
        let messages = [Message::user("hi")];
        let body = |options: &StreamOptions| build_body(&model, &messages, options, None, &BTreeMap::new()).unwrap();
        let options = StreamOptions {
            reasoning: Some(ThinkingLevel::Medium),
            reasoning_summary: Some(ReasoningSummary::Off),
            ..Default::default()
        };
        assert_eq!(body(&options)["reasoning"], json!({"effort": "medium", "summary": "off"}));
        let summary_only = StreamOptions { reasoning_summary: Some(ReasoningSummary::On), ..Default::default() };
        assert_eq!(body(&summary_only)["reasoning"], json!({"effort": "none"}));
    }

    #[test]
    fn additional_tools_models_declare_tools_in_the_input() {
        let mut model = test_model("openai-codex-responses", "openai-codex", "https://chatgpt.com/backend-api");
        model.compat = Some(json!({"supportsMidConvoSystemMessages": true, "supportsAdditionalTools": true}));
        let tool = crate::types::Tool::new("read", "d", json!({"type": "object", "properties": {}}));
        let leading = crate::transcript::create_initial_system_message(Some("prompt"), &[tool]).unwrap();
        let messages = [Message::System(leading), Message::user("hi")];
        let body = build_body(&model, &messages, &StreamOptions::default(), None, &BTreeMap::new()).unwrap();
        assert!(body.get("tools").is_none());
        assert_eq!(body["instructions"], "prompt");
        assert_eq!(body["input"][0]["type"], "additional_tools");
        assert_eq!(body["input"][0]["tools"][0]["name"], "read");
        assert_eq!(body["input"].as_array().unwrap().len(), 2);
    }
}
