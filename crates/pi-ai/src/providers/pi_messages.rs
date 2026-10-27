//! pi-messages (`POST {baseUrl}/messages`): pi's own message protocol, spoken by the Radius
//! gateway. The request carries `{model, context, options}`; the response streams serialized
//! assistant-message events and ends with `done` or `error`.

#[cfg(test)]
mod tests;

use futures::StreamExt;
use serde_json::{Map, Value, json};

use crate::providers::common::HttpRequest;
use crate::providers::http_stream::{ResponseReport, SendConfig, hooked_events, send, status_text};
use crate::registry::{ApiProvider, CacheRetention, StreamOptions};
use crate::request::{apply_payload_hook, build_headers};
use crate::retry::ProviderRequestError;
use crate::simple_options::provider_env_value;
use crate::stream::{AssistantMessageEvent, AssistantMessageEventStream, EventSender, event_channel};
use crate::transcript::normalize_context;
use crate::types::{
    AssistantMessage, ContentBlock, Context, Message, Model, StopReason, ThinkingLevel, ToolCall, Usage, now_ms,
};

const MAX_DIAGNOSTIC_BODY_CHARS: usize = 8192;

pub struct PiMessages;

impl ApiProvider for PiMessages {
    fn supports_system_messages(&self) -> bool {
        true
    }

    fn stream(&self, model: &Model, context: &Context, options: StreamOptions) -> AssistantMessageEventStream {
        let (sender, stream) = event_channel();
        let Some(api_key) = options.api_key.clone().filter(|key| !key.is_empty()) else {
            let message = format!("No API key provided for provider \"{}\"", model.provider);
            sender.push(error_event(model, message, false, Vec::new()));
            return stream;
        };
        let payload = build_payload(model, context, &options);
        let mut defaults = crate::identity::identity_headers(model, options.identity.as_ref());
        defaults.push(("authorization", format!("Bearer {api_key}")));
        defaults.push(("accept", "text/event-stream".to_string()));
        defaults.push(("content-type", "application/json".to_string()));
        let headers = build_headers(defaults, &Default::default(), &options.headers, &options.suppressed_headers);
        let url = messages_url(model, options.debug);
        let mut request = HttpRequest { url, headers, body: payload };
        let model = model.clone();
        tokio::spawn(async move {
            let payload = std::mem::take(&mut request.body);
            request.body = apply_payload_hook(payload, options.on_payload.as_ref(), &model).await;
            let cancel = options.cancel.clone();
            let result = tokio::select! {
                biased;
                _ = cancel.cancelled() => Err((String::new(), Vec::new())),
                result = run(&sender, &model, &options, &request) => result,
            };
            match result {
                _ if cancel.is_cancelled() => {
                    sender.push(error_event(&model, "Request was aborted".into(), true, Vec::new()))
                }
                Ok(()) => {}
                Err((message, diagnostics)) => sender.push(error_event(&model, message, false, diagnostics)),
            }
        });
        stream
    }
}

/// `{baseUrl}/messages`, with `debug=1` set in the query when debug metadata is requested.
fn messages_url(model: &Model, debug: bool) -> String {
    let url = format!("{}/messages", model.base_url.trim_end_matches('/'));
    let Some(mut parsed) = debug.then(|| reqwest::Url::parse(&url).ok()).flatten() else { return url };
    let pairs: Vec<(String, String)> = parsed
        .query_pairs()
        .filter(|(name, _)| name != "debug")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    parsed.query_pairs_mut().clear().extend_pairs(pairs).append_pair("debug", "1");
    parsed.to_string()
}

/// `{model, context, options}`, where `context` is the transcript with a leading system message.
fn build_payload(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    let messages: Vec<Message> =
        normalize_context(context).into_iter().filter(|message| !matches!(message, Message::Custom(_))).collect();
    let mut request = Map::new();
    if let Some(temperature) = options.temperature {
        request.insert("temperature".into(), json!(temperature));
    }
    if let Some(max_tokens) = options.max_tokens {
        request.insert("maxTokens".into(), json!(max_tokens));
    }
    if let Some(reasoning) = options.reasoning.filter(|level| *level != ThinkingLevel::Off) {
        request.insert("reasoning".into(), json!(reasoning));
    }
    let long = provider_env_value("PI_CACHE_RETENTION", &options.env).is_some_and(|value| value == "long");
    if let Some(retention) = options.cache_retention.or(long.then_some(CacheRetention::Long)) {
        request.insert("cacheRetention".into(), json!(retention));
    }
    if let Some(session) = &options.session_id {
        request.insert("sessionId".into(), json!(session));
    }
    if let Some(choice) = options.tool_choice.as_deref() {
        let choice = match choice {
            "auto" | "none" | "required" => json!(choice),
            name => json!({"type": "function", "function": {"name": name}}),
        };
        request.insert("toolChoice".into(), choice);
    }
    json!({"model": model.id, "context": {"messages": messages}, "options": request})
}

type Failure = (String, Vec<Value>);

async fn run(
    sender: &EventSender,
    model: &Model,
    options: &StreamOptions,
    request: &HttpRequest,
) -> Result<(), Failure> {
    let config = SendConfig { max_retries: 0, timeout: None, report: ResponseReport::Always };
    let response =
        send(request, model, options, &config).await.map_err(|error| response_failure(model, &request.url, error))?;
    let mut converter = Converter::new(model);
    let mut events = std::pin::pin!(hooked_events(response, model, options));
    while let Some(event) = events.next().await {
        let event = event.map_err(|error| (error, Vec::new()))?;
        let data = event.data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let wire: Value =
            serde_json::from_str(data).map_err(|error| (format!("Invalid pi-messages event: {error}"), Vec::new()))?;
        check_content_index(&wire, converter.partial.content.len()).map_err(|error| (error, Vec::new()))?;
        if let Some(event) = converter.convert(&wire) {
            let terminal = event.final_message().is_some();
            sender.push(event);
            if terminal {
                return Ok(());
            }
        }
    }
    Err((format!("{} stream ended without a terminal event", model.provider), Vec::new()))
}

/// `<status> <statusText>: <message or body> (<code>)` and a `pi_messages_response_failure` diagnostic.
fn response_failure(model: &Model, url: &str, error: ProviderRequestError) -> Failure {
    let (Some(status), Some(body)) = (error.status, error.body.as_deref()) else { return (error.message, Vec::new()) };
    let parsed = serde_json::from_str::<Value>(body).ok().filter(|value| value["error"].is_object());
    let detail = parsed.as_ref().map(|value| &value["error"]);
    let code = detail.and_then(|detail| detail["code"].as_str());
    let text = detail.and_then(|detail| detail["message"].as_str()).unwrap_or(body);
    let code_suffix = code.map(|code| format!(" ({code})")).unwrap_or_default();
    let message = format!("{status} {}: {text}{code_suffix}", status_text(status));
    let mut details = json!({
        "version": 1, "provider": model.provider, "model": model.id, "url": url,
        "status": status, "statusText": status_text(status),
    });
    match detail {
        Some(detail) => details["error"] = detail.clone(),
        None => details["body"] = json!(truncate_diagnostic(body)),
    }
    details["timestampMs"] = json!(now_ms());
    let mut error = json!({"name": "PiMessagesResponseError", "message": message});
    if let Some(code) = code {
        error["code"] = json!(code);
    }
    let diagnostic =
        json!({"type": "pi_messages_response_failure", "timestamp": now_ms(), "error": error, "details": details});
    (message, vec![diagnostic])
}

fn truncate_diagnostic(body: &str) -> String {
    match body.char_indices().nth(MAX_DIAGNOSTIC_BODY_CHARS) {
        Some((index, _)) => format!("{}…", &body[..index]),
        None => body.to_string(),
    }
}

/// The wire `contentIndex` keys the content blocks, so a huge index would pad billions of blocks.
/// A start adds the next block; other events need an existing one.
fn check_content_index(event: &Value, blocks: usize) -> Result<(), String> {
    let Some(index) = event.get("contentIndex") else { return Ok(()) };
    let kind = event["type"].as_str().unwrap_or_default();
    let limit = if kind.ends_with("_start") { Some(blocks) } else { blocks.checked_sub(1) };
    let integer = index.as_u64().or_else(|| index.as_f64().filter(|n| *n >= 0.0 && n.fract() == 0.0).map(|n| n as u64));
    match (integer, limit) {
        (Some(index), Some(limit)) if index <= limit as u64 => Ok(()),
        _ => Err(format!("Invalid contentIndex {index} in pi-messages {kind} event")),
    }
}

fn error_event(model: &Model, message: String, aborted: bool, diagnostics: Vec<Value>) -> AssistantMessageEvent {
    let reason = if aborted { StopReason::Aborted } else { StopReason::Error };
    let mut error = AssistantMessage::empty(model);
    error.stop_reason = reason;
    error.error_message = Some(message);
    error.diagnostics = diagnostics;
    AssistantMessageEvent::Error { reason, error }
}

/// Rebuilds the partial message from wire events.
struct Converter {
    partial: AssistantMessage,
    tool_json: std::collections::HashMap<usize, String>,
}

impl Converter {
    fn new(model: &Model) -> Self {
        Self { partial: AssistantMessage::empty(model), tool_json: Default::default() }
    }

    /// The block at `index`, padding with empty text blocks for indices the wire skipped.
    fn slot(&mut self, index: usize) -> &mut ContentBlock {
        while self.partial.content.len() <= index {
            self.partial.content.push(ContentBlock::text(""));
        }
        &mut self.partial.content[index]
    }

    fn finish(&mut self, event: &Value) {
        self.partial.usage = serde_json::from_value::<Usage>(event["usage"].clone()).unwrap_or_default();
        self.partial.response_id = event["responseId"].as_str().map(str::to_string);
        if let Some(model) = event["responseModel"].as_str() {
            self.partial.response_model = Some(model.to_string());
        }
        if let Some(level) = event["providerThinkingLevel"].as_str() {
            self.partial.provider_thinking_level = Some(level.to_string());
        }
        if let Some(rewrite) = event.get("rewrite").filter(|rewrite| rewrite.is_object()) {
            let diagnostic = json!({"type": "pi_messages_rewrite", "timestamp": now_ms(), "details": rewrite});
            self.partial.diagnostics.push(diagnostic);
        }
    }

    fn convert(&mut self, event: &Value) -> Option<AssistantMessageEvent> {
        let content_index = event["contentIndex"].as_u64().unwrap_or_default() as usize;
        let text = |key: &str| event[key].as_str().unwrap_or_default().to_string();
        let kind = event["type"].as_str()?;
        match kind {
            "done" | "error" => {
                let reason = serde_json::from_value(event["reason"].clone()).unwrap_or(StopReason::Error);
                self.partial.stop_reason = reason;
                self.finish(event);
                if kind == "error" {
                    self.partial.error_message = event["errorMessage"].as_str().map(str::to_string);
                    return Some(AssistantMessageEvent::Error { reason, error: self.partial.clone() });
                }
                return Some(AssistantMessageEvent::Done { reason, message: self.partial.clone() });
            }
            "text_start" => *self.slot(content_index) = ContentBlock::text(""),
            "thinking_start" => {
                *self.slot(content_index) =
                    ContentBlock::Thinking { thinking: String::new(), thinking_signature: None, redacted: false }
            }
            "text_delta" | "thinking_delta" => match self.slot(content_index) {
                ContentBlock::Text { text: body, .. } | ContentBlock::Thinking { thinking: body, .. } => {
                    body.push_str(&text("delta"))
                }
                _ => {}
            },
            "text_end" => {
                let signature = event["contentSignature"].as_str().map(str::to_string);
                *self.slot(content_index) = ContentBlock::Text { text: text("content"), text_signature: signature };
            }
            "thinking_end" => {
                *self.slot(content_index) = ContentBlock::Thinking {
                    thinking: text("content"),
                    thinking_signature: event["contentSignature"].as_str().map(str::to_string),
                    redacted: event["redacted"] == true,
                };
            }
            "toolcall_start" => {
                let call = ToolCall { id: text("id"), name: text("toolName"), ..ToolCall::default() };
                *self.slot(content_index) = ContentBlock::ToolCall(call);
                self.tool_json.insert(content_index, String::new());
            }
            "toolcall_delta" => {
                let json = self.tool_json.entry(content_index).or_default();
                json.push_str(&text("delta"));
                let parsed = crate::json::parse_streaming_json(json);
                if let ContentBlock::ToolCall(call) = self.slot(content_index) {
                    call.arguments = match parsed {
                        Some(Value::Object(arguments)) => arguments,
                        _ => Default::default(),
                    };
                }
            }
            "toolcall_end" => {
                self.tool_json.remove(&content_index);
                let tool_call: ToolCall = serde_json::from_value(event["toolCall"].clone()).unwrap_or_default();
                *self.slot(content_index) = ContentBlock::ToolCall(tool_call.clone());
                let partial = self.partial.clone();
                return Some(AssistantMessageEvent::ToolcallEnd { content_index, tool_call, partial });
            }
            "start" => {}
            _ => return None,
        }
        let partial = self.partial.clone();
        Some(match kind {
            "start" => AssistantMessageEvent::Start { partial },
            "text_start" => AssistantMessageEvent::TextStart { content_index, partial },
            "text_delta" => AssistantMessageEvent::TextDelta { content_index, delta: text("delta"), partial },
            "text_end" => AssistantMessageEvent::TextEnd { content_index, content: text("content"), partial },
            "thinking_start" => AssistantMessageEvent::ThinkingStart { content_index, partial },
            "thinking_delta" => AssistantMessageEvent::ThinkingDelta { content_index, delta: text("delta"), partial },
            "thinking_end" => AssistantMessageEvent::ThinkingEnd { content_index, content: text("content"), partial },
            "toolcall_start" => AssistantMessageEvent::ToolcallStart { content_index, partial },
            _ => AssistantMessageEvent::ToolcallDelta { content_index, delta: text("delta"), partial },
        })
    }
}
