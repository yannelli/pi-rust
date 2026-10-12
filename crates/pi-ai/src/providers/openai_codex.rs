//! OpenAI Codex Responses (`{baseUrl}/codex/responses`) for ChatGPT accounts, over a
//! WebSocket (`websocket`, `websocket-cached`, or `auto`) with SSE fallback, or SSE alone.

mod events;
mod request;
mod sse;
#[cfg(test)]
mod tests;
mod websocket;
#[cfg(test)]
mod ws_tests;

use std::collections::BTreeMap;

use futures::{SinkExt, StreamExt};
use serde_json::{Map, Value, json};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use self::events::{CodexError, CodexEvents};
use self::request::{Compat, HeaderList, build_body, cache_session_id, codex_input, extract_account_id};
use self::request::{resolve_codex_websocket_url, sse_headers, uuid_v7, websocket_headers};
pub use self::websocket::{
    CodexWebSocketDebugStats, close_websocket_sessions, reset_websocket_debug_stats, websocket_debug_stats,
};
use self::websocket::{Continuation, DEFAULT_CONNECT_TIMEOUT_MS, Lease};
use crate::providers::constrained_sampling::grammar_tool_input_properties;
use crate::providers::openai_responses::ResponsesStream;
use crate::registry::{ApiProvider, StreamOptions, Transport};
use crate::stream::{AssistantMessageEventStream, MessageBuilder, diagnostic, error_stream, event_channel};
use crate::transcript::{declared_tools, normalize_context, resolve_transcript};
use crate::types::{Context, Message, Model, StopReason};

const CONNECTION_LIMIT_REACHED: &str = "websocket_connection_limit_reached";
const PREVIOUS_RESPONSE_NOT_FOUND: &str = "previous_response_not_found";

pub struct OpenAICodexResponses;

impl ApiProvider for OpenAICodexResponses {
    fn supports_system_messages(&self) -> bool {
        true
    }

    fn stream(&self, model: &Model, context: &Context, options: StreamOptions) -> AssistantMessageEventStream {
        let Some(token) = options.api_key.clone().filter(|key| !key.is_empty()) else {
            return error_stream(model, StopReason::Error, format!("No API key for provider: {}", model.provider));
        };
        let (sender, stream) = event_channel();
        let mut builder = MessageBuilder::new(model, sender);
        let (model, context) = (model.clone(), context.clone());
        tokio::spawn(async move {
            let cancel = options.cancel.clone();
            let result = tokio::select! {
                biased;
                _ = cancel.cancelled() => Err(CodexError::Transport(String::new())),
                result = execute(&model, &context, &options, &token, &mut builder) => result,
            };
            if cancel.is_cancelled() {
                return builder.fail(StopReason::Aborted, "Request was aborted");
            }
            match result {
                Ok(reason) => builder.finish(reason),
                Err(error) => builder.fail(StopReason::Error, error.message()),
            }
        });
        stream
    }
}

/// Everything both transports need for one request.
struct Prepared {
    model: Model,
    body: Value,
    body_json: String,
    sse_headers: HeaderList,
    ws_headers: HeaderList,
    account: String,
    session: Option<String>,
    compat: Compat,
    grammar: BTreeMap<String, String>,
}

fn prepare(model: &Model, context: &Context, options: &StreamOptions, token: &str) -> Result<Prepared, CodexError> {
    let transport = |message: String| CodexError::Transport(message);
    let account = extract_account_id(token).map_err(transport)?;
    let compat = Compat::resolve(model);
    let messages = resolve_transcript(normalize_context(context), compat.supports_mid_convo_system_messages);
    let grammar = grammar_tool_input_properties(&declared_tools(&messages), compat.supports_grammar_tools)
        .map_err(transport)?
        .into_iter()
        .collect();
    let session = cache_session_id(options);
    let mut body = build_body(model, &messages, options, session.as_deref(), &grammar).map_err(transport)?;
    if let Some(replacement) = options.on_payload.as_ref().and_then(|hook| hook(&body, model)) {
        body = replacement;
    }
    let request_id = session.clone().unwrap_or_else(uuid_v7);
    Ok(Prepared {
        model: model.clone(),
        body_json: body.to_string(),
        body,
        sse_headers: sse_headers(model, options, &account, token, session.as_deref()),
        ws_headers: websocket_headers(model, options, &account, token, &request_id),
        account,
        session,
        compat,
        grammar,
    })
}

fn new_events(prepared: &Prepared, options: &StreamOptions, started: bool) -> CodexEvents {
    let inner = ResponsesStream::new(&prepared.model).with_grammar_tool_input_properties(prepared.grammar.clone());
    CodexEvents::new(inner, &prepared.model, options.on_provider_stream_event.clone(), started)
}

async fn execute(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    token: &str,
    builder: &mut MessageBuilder,
) -> Result<StopReason, CodexError> {
    let prepared = prepare(model, context, options, token)?;
    let session = prepared.session.as_deref();
    let transport = options.transport.unwrap_or(Transport::Auto);
    let mut started = false;
    let disabled = transport != Transport::Sse && websocket::sse_fallback_active(session);
    if disabled {
        websocket::record_sse_fallback(session);
    }
    if transport != Transport::Sse && !disabled {
        let (mut retried_limit, mut retried_missing) = (false, false);
        loop {
            let mut events = new_events(&prepared, options, started);
            let result = websocket_request(&prepared, &mut events, builder, options).await;
            started = events.started;
            let error = match result {
                Ok(reason) => return Ok(reason),
                Err(error) => error,
            };
            let limit_before_start = !events.seen && error.code() == Some(CONNECTION_LIMIT_REACHED);
            if error.code() == Some(PREVIOUS_RESPONSE_NOT_FOUND) && !retried_missing {
                retried_missing = true;
                continue;
            }
            if limit_before_start && !retried_limit {
                retried_limit = true;
                continue;
            }
            if !error.is_transport() && !limit_before_start {
                return Err(error);
            }
            let mut details = Map::new();
            details.insert("configuredTransport".into(), serde_json::to_value(transport).unwrap_or_default());
            if !events.seen {
                details.insert("fallbackTransport".into(), json!("sse"));
            }
            details.insert("eventsEmitted".into(), json!(events.seen));
            let phase = if events.seen { "after_message_stream_start" } else { "before_message_stream_start" };
            details.insert("phase".into(), json!(phase));
            details.insert("requestBytes".into(), json!(prepared.body_json.len()));
            builder.add_diagnostic(diagnostic("provider_transport_failure", Some(error.message()), Some(details)));
            websocket::record_failure(session, error.message());
            if events.seen {
                return Err(error);
            }
            websocket::record_sse_fallback(session);
            break;
        }
    }
    let mut events = new_events(&prepared, options, started);
    sse::request(&prepared, &mut events, builder, options).await
}

fn without_input(body: &Value) -> Value {
    let mut body = body.clone();
    if let Some(object) = body.as_object_mut() {
        object.remove("input");
        object.remove("previous_response_id");
    }
    body
}

/// The body with only new input and `previous_response_id`, when the cached socket's last
/// request is a prefix of this one; otherwise `None` and the full body is sent.
fn continuation_body(continuation: &Continuation, body: &Value) -> Option<Value> {
    if without_input(body) != without_input(&continuation.last_request_body) || continuation.last_response_id.is_empty()
    {
        return None;
    }
    let current = body.get("input").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut baseline =
        continuation.last_request_body.get("input").and_then(Value::as_array).cloned().unwrap_or_default();
    baseline.extend(continuation.last_response_items.iter().cloned());
    if current.len() < baseline.len() || current[..baseline.len()] != baseline[..] {
        return None;
    }
    let mut next = body.clone();
    next["previous_response_id"] = json!(continuation.last_response_id);
    next["input"] = Value::Array(current[baseline.len()..].to_vec());
    Some(next)
}

fn close_message(frame: Option<tokio_tungstenite::tungstenite::protocol::CloseFrame>) -> String {
    let Some(frame) = frame else { return "WebSocket closed".into() };
    let code = u16::from(frame.code);
    let reason = match frame.reason.as_str() {
        "" if code == 1009 => " message too big".to_string(),
        "" => String::new(),
        reason => format!(" {reason}"),
    };
    format!("WebSocket closed {code}{reason}")
}

async fn websocket_request(
    prepared: &Prepared,
    events: &mut CodexEvents,
    builder: &mut MessageBuilder,
    options: &StreamOptions,
) -> Result<StopReason, CodexError> {
    let url = resolve_codex_websocket_url(&prepared.model.base_url);
    let connect_timeout = options.websocket_connect_timeout_ms.unwrap_or(DEFAULT_CONNECT_TIMEOUT_MS);
    let session = prepared.session.as_deref();
    let mut lease = websocket::acquire(&url, &prepared.ws_headers, session, &prepared.account, connect_timeout).await?;
    // Upstream checks the configured transport, so an unset (default auto) transport sends full context.
    let cached_context = matches!(options.transport, Some(Transport::WebsocketCached | Transport::Auto));
    let previous = lease.continuation.take();
    let delta =
        previous.as_ref().filter(|_| cached_context && lease.cached).and_then(|c| continuation_body(c, &prepared.body));
    let request_body = delta.unwrap_or_else(|| prepared.body.clone());
    record_request_stats(session, &lease, cached_context, &request_body);
    let result = stream_response(&mut lease, &request_body, events, builder, options).await;
    match result {
        Ok(reason) => {
            let continuation = (cached_context && lease.cached)
                .then(|| builder.message().response_id.clone())
                .flatten()
                .map(|response_id| Continuation {
                    last_request_body: prepared.body.clone(),
                    last_response_id: response_id,
                    last_response_items: response_items(prepared, builder),
                });
            lease.release(true, continuation);
            Ok(reason)
        }
        Err(error) => {
            lease.release(false, None);
            Err(error)
        }
    }
}

/// Output items of this response as they appear in the next request's input.
fn response_items(prepared: &Prepared, builder: &MessageBuilder) -> Vec<Value> {
    let output = [Message::Assistant(builder.message().clone())];
    codex_input(&prepared.model, &output, prepared.compat, &prepared.grammar)
        .into_iter()
        .filter(|item| !matches!(item["type"].as_str(), Some("function_call_output" | "custom_tool_call_output")))
        .collect()
}

fn record_request_stats(session: Option<&str>, lease: &Lease, cached_context: bool, body: &Value) {
    websocket::with_stats(session, |stats| {
        stats.requests += 1;
        if lease.reused {
            stats.connections_reused += 1;
        } else {
            stats.connections_created += 1;
        }
        if cached_context {
            stats.cached_context_requests += 1;
        }
        if body["store"] == true {
            stats.store_true_requests += 1;
        }
        let items = body["input"].as_array().map_or(0, Vec::len);
        stats.last_input_items = items;
        match body["previous_response_id"].as_str() {
            Some(previous) => {
                stats.delta_requests += 1;
                stats.last_delta_input_items = Some(items);
                stats.last_previous_response_id = Some(previous.to_string());
            }
            None => {
                stats.full_context_requests += 1;
                stats.last_delta_input_items = None;
                stats.last_previous_response_id = None;
            }
        }
    });
}

async fn stream_response(
    lease: &mut Lease,
    body: &Value,
    events: &mut CodexEvents,
    builder: &mut MessageBuilder,
    options: &StreamOptions,
) -> Result<StopReason, CodexError> {
    let transport = |message: String| CodexError::Transport(message);
    let mut message = Map::new();
    message.insert("type".into(), json!("response.create"));
    message.extend(body.as_object().cloned().unwrap_or_default());
    let text = Value::Object(message).to_string();
    lease.socket().send(WsMessage::Text(text.into())).await.map_err(|error| transport(error.to_string()))?;
    let idle = options.timeout_ms.filter(|timeout| *timeout > 0);
    loop {
        let next = match idle {
            Some(timeout) => tokio::time::timeout(std::time::Duration::from_millis(timeout), lease.socket().next())
                .await
                .map_err(|_| transport(format!("WebSocket idle timeout after {timeout}ms")))?,
            None => lease.socket().next().await,
        };
        let text = match next {
            None => return Err(transport("WebSocket stream closed before response.completed".into())),
            Some(Err(error)) => return Err(transport(error.to_string())),
            Some(Ok(WsMessage::Text(text))) => text.to_string(),
            Some(Ok(WsMessage::Binary(bytes))) => String::from_utf8_lossy(&bytes).into_owned(),
            Some(Ok(WsMessage::Close(frame))) => return Err(transport(close_message(frame))),
            Some(Ok(_)) => continue,
        };
        if !text.is_empty() && events.event(builder, &text, "WebSocket")? {
            break;
        }
    }
    events.end(builder)
}
