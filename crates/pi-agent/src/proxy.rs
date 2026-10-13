//! Stream function for apps that route model calls through a server that holds the credentials
//! (upstream `streamProxy`). The server strips `partial` from events to save bandwidth; the client
//! rebuilds the partial message.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use futures::StreamExt;
use pi_ai::json::parse_streaming_json;
use pi_ai::stream::EventSender;
use pi_ai::transcript::normalize_context;
use pi_ai::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, CacheRetention, ContentBlock, Context, Model,
    StopReason, StreamOptions, ThinkingBudgets, ThinkingLevel, ToolCall, Transport, Usage, event_channel,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::agent_loop::StreamFn;

/// Events as the proxy server sends them, without `partial`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum ProxyAssistantMessageEvent {
    Start,
    TextStart { content_index: usize },
    TextDelta { content_index: usize, delta: String },
    TextEnd { content_index: usize, content_signature: Option<String> },
    ThinkingStart { content_index: usize },
    ThinkingDelta { content_index: usize, delta: String },
    ThinkingEnd { content_index: usize, content_signature: Option<String> },
    ToolcallStart { content_index: usize, id: String, tool_name: String },
    ToolcallDelta { content_index: usize, delta: String },
    ToolcallEnd { content_index: usize, tool_call: ToolCall },
    Done { reason: StopReason, usage: Usage, provider_thinking_level: Option<String> },
    Error { reason: StopReason, error_message: Option<String>, usage: Usage, provider_thinking_level: Option<String> },
}

/// Options for [`stream_proxy`]: the proxy endpoint and the request options it forwards.
#[derive(Clone, Debug, Default)]
pub struct ProxyStreamOptions {
    /// Proxy server URL, for example `https://genai.example.com`.
    pub proxy_url: String,
    /// Bearer token for the proxy server.
    pub auth_token: String,
    pub cancel: CancellationToken,
    pub temperature: Option<f64>,
    pub sampling_params: Option<Map<String, Value>>,
    pub max_tokens: Option<u64>,
    pub reasoning: Option<ThinkingLevel>,
    pub cache_retention: Option<CacheRetention>,
    pub session_id: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub metadata: Option<Map<String, Value>>,
    pub transport: Option<Transport>,
    pub thinking_budgets: Option<ThinkingBudgets>,
    pub max_retry_delay_ms: Option<u64>,
}

impl ProxyStreamOptions {
    /// The forwardable parts of the loop's request options.
    pub fn from_stream_options(
        options: &StreamOptions,
        proxy_url: impl Into<String>,
        auth_token: impl Into<String>,
    ) -> Self {
        Self {
            proxy_url: proxy_url.into(),
            auth_token: auth_token.into(),
            cancel: options.cancel.clone(),
            temperature: options.temperature,
            sampling_params: options.sampling_params.clone(),
            max_tokens: options.max_tokens,
            reasoning: options.reasoning,
            cache_retention: options.cache_retention,
            session_id: options.session_id.clone(),
            headers: options.headers.clone(),
            metadata: options.metadata.clone(),
            transport: options.transport,
            thinking_budgets: options.thinking_budgets,
            max_retry_delay_ms: options.max_retry_delay_ms,
        }
    }

    fn request_options(&self) -> Value {
        let mut options = Map::new();
        let mut put = |key: &str, value: Value| {
            if !value.is_null() {
                options.insert(key.into(), value);
            }
        };
        put("temperature", json!(self.temperature));
        put("samplingParams", json!(self.sampling_params));
        put("maxTokens", json!(self.max_tokens));
        put("reasoning", json!(self.reasoning));
        put("cacheRetention", json!(self.cache_retention));
        put("sessionId", json!(self.session_id));
        if !self.headers.is_empty() {
            put("headers", json!(self.headers));
        }
        put("metadata", json!(self.metadata));
        put("transport", json!(self.transport));
        let budgets = serde_json::to_value(self.thinking_budgets).unwrap_or(Value::Null);
        put("thinkingBudgets", strip_nulls(budgets));
        put("maxRetryDelayMs", json!(self.max_retry_delay_ms));
        Value::Object(options)
    }
}

fn strip_nulls(value: Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(object.into_iter().filter(|(_, value)| !value.is_null()).collect()),
        value => value,
    }
}

/// A [`StreamFn`] that sends every request through the proxy. `auth_token` is read per request.
pub fn proxy_stream_fn(proxy_url: impl Into<String>, auth_token: Arc<dyn Fn() -> String + Send + Sync>) -> StreamFn {
    let proxy_url = proxy_url.into();
    Arc::new(move |model: &Model, context: &Context, options: StreamOptions| {
        stream_proxy(model, context, ProxyStreamOptions::from_stream_options(&options, proxy_url.clone(), auth_token()))
    })
}

/// Rebuilds the partial message from proxy events.
struct PartialMessage {
    message: AssistantMessage,
    /// Raw argument JSON of tool calls still streaming, by content index.
    tool_json: HashMap<usize, String>,
}

impl PartialMessage {
    fn set_block(&mut self, index: usize, block: ContentBlock) {
        while self.message.content.len() < index {
            self.message.content.push(ContentBlock::text(""));
        }
        match self.message.content.get_mut(index) {
            Some(slot) => *slot = block,
            None => self.message.content.push(block),
        }
    }

    fn apply(&mut self, event: ProxyAssistantMessageEvent) -> Result<Option<AssistantMessageEvent>, String> {
        use ProxyAssistantMessageEvent as Proxy;
        let event = match event {
            Proxy::Start => AssistantMessageEvent::Start { partial: self.message.clone() },
            Proxy::TextStart { content_index } => {
                self.set_block(content_index, ContentBlock::text(""));
                AssistantMessageEvent::TextStart { content_index, partial: self.message.clone() }
            }
            Proxy::TextDelta { content_index, delta } => match self.message.content.get_mut(content_index) {
                Some(ContentBlock::Text { text, .. }) => {
                    text.push_str(&delta);
                    AssistantMessageEvent::TextDelta { content_index, delta, partial: self.message.clone() }
                }
                _ => return Err("Received text_delta for non-text content".into()),
            },
            Proxy::TextEnd { content_index, content_signature } => match self.message.content.get_mut(content_index) {
                Some(ContentBlock::Text { text, text_signature }) => {
                    *text_signature = content_signature;
                    let content = text.clone();
                    AssistantMessageEvent::TextEnd { content_index, content, partial: self.message.clone() }
                }
                _ => return Err("Received text_end for non-text content".into()),
            },
            Proxy::ThinkingStart { content_index } => {
                let block =
                    ContentBlock::Thinking { thinking: String::new(), thinking_signature: None, redacted: false };
                self.set_block(content_index, block);
                AssistantMessageEvent::ThinkingStart { content_index, partial: self.message.clone() }
            }
            Proxy::ThinkingDelta { content_index, delta } => match self.message.content.get_mut(content_index) {
                Some(ContentBlock::Thinking { thinking, .. }) => {
                    thinking.push_str(&delta);
                    AssistantMessageEvent::ThinkingDelta { content_index, delta, partial: self.message.clone() }
                }
                _ => return Err("Received thinking_delta for non-thinking content".into()),
            },
            Proxy::ThinkingEnd { content_index, content_signature } => {
                match self.message.content.get_mut(content_index) {
                    Some(ContentBlock::Thinking { thinking, thinking_signature, .. }) => {
                        *thinking_signature = content_signature;
                        let content = thinking.clone();
                        AssistantMessageEvent::ThinkingEnd { content_index, content, partial: self.message.clone() }
                    }
                    _ => return Err("Received thinking_end for non-thinking content".into()),
                }
            }
            Proxy::ToolcallStart { content_index, id, tool_name } => {
                let call = ToolCall { id, name: tool_name, ..Default::default() };
                self.set_block(content_index, ContentBlock::ToolCall(call));
                self.tool_json.insert(content_index, String::new());
                AssistantMessageEvent::ToolcallStart { content_index, partial: self.message.clone() }
            }
            Proxy::ToolcallDelta { content_index, delta } => match self.message.content.get_mut(content_index) {
                Some(ContentBlock::ToolCall(call)) => {
                    let json = self.tool_json.entry(content_index).or_default();
                    json.push_str(&delta);
                    call.arguments =
                        parse_streaming_json(json).and_then(|value| value.as_object().cloned()).unwrap_or_default();
                    AssistantMessageEvent::ToolcallDelta { content_index, delta, partial: self.message.clone() }
                }
                _ => return Err("Received toolcall_delta for non-toolCall content".into()),
            },
            Proxy::ToolcallEnd { content_index, tool_call } => match self.message.content.get_mut(content_index) {
                Some(ContentBlock::ToolCall(call)) => {
                    // The end event carries the complete call, including fields such as `namespace`.
                    *call = tool_call.clone();
                    self.tool_json.remove(&content_index);
                    AssistantMessageEvent::ToolcallEnd { content_index, tool_call, partial: self.message.clone() }
                }
                _ => return Ok(None),
            },
            Proxy::Done { reason, usage, provider_thinking_level } => {
                self.message.stop_reason = reason;
                self.message.usage = usage;
                if provider_thinking_level.is_some() {
                    self.message.provider_thinking_level = provider_thinking_level;
                }
                AssistantMessageEvent::Done { reason, message: self.message.clone() }
            }
            Proxy::Error { reason, error_message, usage, provider_thinking_level } => {
                self.message.stop_reason = reason;
                self.message.error_message = error_message;
                self.message.usage = usage;
                if provider_thinking_level.is_some() {
                    self.message.provider_thinking_level = provider_thinking_level;
                }
                AssistantMessageEvent::Error { reason, error: self.message.clone() }
            }
        };
        Ok(Some(event))
    }

    fn fail(&mut self, sender: &EventSender, reason: StopReason, message: String) {
        self.message.stop_reason = reason;
        self.message.error_message = Some(message);
        sender.push(AssistantMessageEvent::Error { reason, error: self.message.clone() });
    }
}

/// Stream a response through the proxy's `/api/stream` endpoint.
pub fn stream_proxy(model: &Model, context: &Context, options: ProxyStreamOptions) -> AssistantMessageEventStream {
    let (sender, stream) = event_channel();
    let mut partial = PartialMessage { message: AssistantMessage::empty(model), tool_json: HashMap::new() };
    let body = json!({
        "model": model,
        "context": { "messages": normalize_context(context) },
        "options": options.request_options(),
    });
    tokio::spawn(async move {
        let cancel = options.cancel.clone();
        let outcome = tokio::select! {
            outcome = run(&options, body, &mut partial, &sender) => outcome,
            _ = cancel.cancelled() => Err("Request aborted by user".to_string()),
        };
        if let Err(message) = outcome {
            let reason = if cancel.is_cancelled() { StopReason::Aborted } else { StopReason::Error };
            partial.fail(&sender, reason, message);
        }
    });
    stream
}

async fn run(
    options: &ProxyStreamOptions,
    body: Value,
    partial: &mut PartialMessage,
    sender: &EventSender,
) -> Result<(), String> {
    let url = format!("{}/api/stream", options.proxy_url.trim_end_matches('/'));
    let response = pi_ai::http_client()
        .post(url)
        .bearer_auth(&options.auth_token)
        .json(&body)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status();
    if !status.is_success() {
        let mut message = format!("Proxy error: {} {}", status.as_u16(), status.canonical_reason().unwrap_or_default());
        if let Ok(Value::Object(error)) = response.json::<Value>().await
            && let Some(text) = error.get("error").and_then(Value::as_str)
        {
            message = format!("Proxy error: {text}");
        }
        return Err(message);
    }
    let mut chunks = response.bytes_stream();
    let mut buffer: Vec<u8> = Vec::new();
    let mut terminal = false;
    let mut process = |line: &[u8], partial: &mut PartialMessage| -> Result<(), String> {
        let line = String::from_utf8_lossy(line);
        let Some(data) = line.strip_prefix("data: ") else { return Ok(()) };
        let data = data.trim();
        if data.is_empty() {
            return Ok(());
        }
        let event: ProxyAssistantMessageEvent = serde_json::from_str(data).map_err(|error| error.to_string())?;
        if let Some(event) = partial.apply(event)? {
            terminal |= event.final_message().is_some();
            sender.push(event);
        }
        Ok(())
    };
    while let Some(chunk) = chunks.next().await {
        buffer.extend_from_slice(&chunk.map_err(|error| error.to_string())?);
        while let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = buffer.drain(..=position).collect();
            process(&line[..line.len() - 1], partial)?;
        }
    }
    // The last event may lack a trailing newline.
    if !buffer.is_empty() {
        process(&buffer, partial)?;
    }
    if !terminal {
        // A clean end without `done` or `error` means the server dropped the response.
        partial.fail(
            sender,
            StopReason::Error,
            "Connection closed by proxy server before the response completed".into(),
        );
    }
    Ok(())
}
