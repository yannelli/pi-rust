//! Grok Build (`POST {baseUrl}/responses` on the Grok CLI chat proxy) for SuperGrok and X Premium:
//! the OpenAI Responses body rewritten to what the proxy accepts, sent with the Grok Build CLI headers.

#[cfg(test)]
mod tests;

use serde_json::{Map, Value, json};

use crate::providers::common::{HttpRequest, SseHandler, endpoint, header_map_with, spawn_provider_stream};
use crate::providers::openai_responses::{Compat, ResponsesStream, build_body, grammar_properties};
use crate::registry::{ApiProvider, StreamOptions};
use crate::simple_options::provider_env_value;
use crate::sse::SseEvent;
use crate::stream::{AssistantMessageEventStream, MessageBuilder, error_stream};
use crate::transcript::{normalize_context, resolve_transcript};
use crate::types::{Context, Model, StopReason};

pub const GROK_BUILD_BASE_URL: &str = "https://cli-chat-proxy.grok.com/v1";
/// The Grok Build CLI release the proxy accepts; `GROK_CLIENT_VERSION` replaces it.
pub const GROK_CLIENT_VERSION: &str = "1.0.45";
const CLIENT_IDENTIFIER: &str = "grok-shell";
const INSTRUCTION_ROLES: [&str; 2] = ["system", "developer"];
const UNSUPPORTED_PARAMETERS: [&str; 5] =
    ["prompt_cache_retention", "max_output_tokens", "temperature", "top_p", "user"];
const TEXT_PART_TYPES: [&str; 3] = ["input_text", "output_text", "text"];
const ENCRYPTED_REASONING: &str = "reasoning.encrypted_content";

pub struct GrokBuildResponses;

impl ApiProvider for GrokBuildResponses {
    fn supports_system_messages(&self) -> bool {
        true
    }

    fn stream(&self, model: &Model, context: &Context, options: StreamOptions) -> AssistantMessageEventStream {
        let Some(token) = options.api_key.clone().filter(|key| !key.is_empty()) else {
            return error_stream(model, StopReason::Error, format!("No API key for provider: {}", model.provider));
        };
        let compat = Compat::resolve(model);
        let messages = resolve_transcript(normalize_context(context), compat.supports_mid_convo_system_messages);
        let built = grammar_properties(&messages, &compat)
            .and_then(|grammar| Ok((build_body(model, &messages, &options, &compat, &grammar)?, grammar)));
        let (body, grammar) = match built {
            Ok(built) => built,
            Err(error) => return error_stream(model, StopReason::Error, error),
        };
        let version = client_version(&options);
        let mut headers = grok_cli_headers(&token, &version);
        headers.push(("x-grok-model-override", model.id.clone()));
        if let Some(session) = options.session_id.clone().filter(|session| !session.is_empty()) {
            headers.push(("x-grok-conv-id", session));
        }
        let request = HttpRequest {
            url: endpoint(&model.base_url, "/responses"),
            headers: header_map_with(Vec::new(), headers, model, &options),
            body: rewrite_body(body),
        };
        let inner = ResponsesStream::new(model).with_grammar_tool_input_properties(grammar);
        spawn_provider_stream(model, &options, request, GrokBuildStream { inner, version })
    }
}

/// `GROK_CLIENT_VERSION` from the provider env or the process, else [`GROK_CLIENT_VERSION`].
pub fn client_version(options: &StreamOptions) -> String {
    provider_env_value("GROK_CLIENT_VERSION", &options.env).unwrap_or_else(|| GROK_CLIENT_VERSION.to_string())
}

/// The headers the Grok Build CLI sends with every proxy request.
pub fn grok_cli_headers(token: &str, version: &str) -> Vec<(&'static str, String)> {
    vec![
        ("authorization", format!("Bearer {token}")),
        ("X-XAI-Token-Auth", "xai-grok-cli".into()),
        ("x-grok-client-version", version.into()),
        ("x-grok-client-identifier", CLIENT_IDENTIFIER.into()),
        ("x-grok-client-mode", "cli".into()),
        ("User-Agent", format!("{CLIENT_IDENTIFIER}/{version} (pi)")),
        ("Accept", "text/event-stream".into()),
    ]
}

/// The proxy keeps no responses, so item ids and reasoning without `encrypted_content` are dropped;
/// it also rejects empty text, sampling fields, and strict tools without an object schema.
pub(crate) fn rewrite_body(body: Value) -> Value {
    let Value::Object(mut body) = body else { return body };
    let leading = body.remove("instructions").and_then(|text| text.as_str().map(|text| text.trim().to_string()));
    let mut instructions: Vec<String> = leading.into_iter().collect();
    let mut conversation = Vec::new();
    for item in body.remove("input").and_then(|input| input.as_array().cloned()).unwrap_or_default() {
        match item.get("role").and_then(Value::as_str) {
            Some(role) if INSTRUCTION_ROLES.contains(&role) => instructions.push(item_text(&item)),
            _ => conversation.push(item),
        }
    }
    instructions.retain(|text| !text.is_empty());
    if !instructions.is_empty() {
        body.insert("instructions".into(), json!(instructions.join("\n\n")));
    }
    body.insert("input".into(), Value::Array(conversation.into_iter().filter_map(normalize_item).collect()));
    body.insert("store".into(), json!(false));
    body.insert("stream".into(), json!(true));
    let mut include: Vec<Value> = body.get("include").and_then(Value::as_array).cloned().unwrap_or_default();
    if !include.iter().any(|value| value == ENCRYPTED_REASONING) {
        include.push(json!(ENCRYPTED_REASONING));
    }
    body.insert("include".into(), Value::Array(include));
    for parameter in UNSUPPORTED_PARAMETERS {
        body.remove(parameter);
    }
    // The Grok CLI sends `reasoning.effort` alone, and none when thinking is off.
    if let Some(Value::Object(reasoning)) = body.get_mut("reasoning") {
        reasoning.remove("summary");
        if reasoning.get("effort").is_none_or(|effort| effort == "none") {
            body.remove("reasoning");
        }
    }
    if let Some(Value::Array(tools)) =
        body.get_mut("tools").filter(|tools| tools.as_array().is_some_and(|t| !t.is_empty()))
    {
        tools.iter_mut().for_each(normalize_tool);
        body.entry("tool_choice").or_insert_with(|| json!("auto"));
    }
    Value::Object(body)
}

fn item_text(item: &Value) -> String {
    match item.get("content") {
        Some(Value::String(text)) => text.trim().to_string(),
        Some(Value::Array(parts)) => {
            let texts = parts.iter().filter_map(|part| part.get("text").and_then(Value::as_str));
            texts.filter(|text| !text.is_empty()).collect::<Vec<_>>().join("\n\n").trim().to_string()
        }
        _ => String::new(),
    }
}

fn normalize_item(item: Value) -> Option<Value> {
    let Value::Object(mut item) = item else { return Some(item) };
    let encrypted = item.get("encrypted_content").and_then(Value::as_str).is_some_and(|content| !content.is_empty());
    if item.get("type").and_then(Value::as_str) == Some("reasoning") && !encrypted {
        return None;
    }
    item.remove("id");
    match item.get_mut("content") {
        Some(Value::String(text)) if text.trim().is_empty() => return None,
        Some(Value::Array(parts)) => {
            parts.retain(|part| !is_empty_text_part(part));
            if parts.is_empty() {
                return None;
            }
        }
        _ => {}
    }
    Some(Value::Object(item))
}

fn is_empty_text_part(part: &Value) -> bool {
    let text_part = part.get("type").and_then(Value::as_str).is_some_and(|kind| TEXT_PART_TYPES.contains(&kind));
    text_part && part.get("text").and_then(Value::as_str).is_none_or(|text| text.trim().is_empty())
}

fn normalize_tool(tool: &mut Value) {
    if tool.get("type").and_then(Value::as_str) != Some("function") {
        return;
    }
    let mut parameters = match tool.get("parameters") {
        Some(Value::Object(parameters)) => parameters.clone(),
        _ => Map::from_iter([("type".to_string(), json!("object"))]),
    };
    if parameters.get("properties").and_then(Value::as_object).is_none_or(Map::is_empty) {
        parameters.insert("properties".into(), json!({}));
    }
    tool["parameters"] = Value::Object(parameters);
    tool["strict"] = json!(false);
}

/// The Responses stream with hints for the proxy's plan and client version rejections.
struct GrokBuildStream {
    inner: ResponsesStream,
    version: String,
}

impl SseHandler for GrokBuildStream {
    fn start(&mut self, builder: &mut MessageBuilder) {
        self.inner.start(builder);
    }

    fn event(&mut self, builder: &mut MessageBuilder, event: SseEvent) -> Result<bool, String> {
        self.inner.event(builder, event)
    }

    fn end(&mut self, builder: &mut MessageBuilder) -> Result<StopReason, String> {
        self.inner.end(builder)
    }

    fn error_message(&self, message: String) -> String {
        let message = self.inner.error_message(message);
        let hint = if message.starts_with("Provider HTTP 426") {
            format!(
                "Grok Build rejected client version {}. Set GROK_CLIENT_VERSION to a current Grok CLI version.",
                self.version
            )
        } else if message.starts_with("Provider HTTP 403") || message.starts_with("Provider HTTP 402") {
            "Grok Build needs a SuperGrok or X Premium plan with Grok Build usage left.".to_string()
        } else {
            return message;
        };
        format!("{message}\n{hint}")
    }
}
