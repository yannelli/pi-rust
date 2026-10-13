//! Codex WebSocket transport against a local server that speaks WebSocket and SSE on one port.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

use super::tests::{codex_model, context, events, mock_token, sse_body};
use super::*;
use crate::providers::mock_server::{cancel_after_first_text, collect, kinds};

#[derive(Clone)]
enum Action {
    Reply(Vec<Value>),
    ReplyThenStall(Vec<Value>),
    Stall,
}

/// `(connection number from 1, request body) -> what the server sends back`.
type Script = Arc<dyn Fn(usize, &Value) -> Action + Send + Sync>;

#[derive(Default)]
struct Seen {
    connections: usize,
    headers: Vec<HashMap<String, String>>,
    bodies: Vec<(usize, Value)>,
    closes: usize,
    http_requests: usize,
}

struct Server {
    base_url: String,
    seen: Arc<Mutex<Seen>>,
}

impl Server {
    async fn start(script: Script, stall_handshake: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let seen: Arc<Mutex<Seen>> = Arc::default();
        let shared = seen.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (script, seen) = (script.clone(), shared.clone());
                tokio::spawn(async move {
                    let head = peek_head(&stream).await.to_ascii_lowercase();
                    if !head.contains("upgrade: websocket") {
                        return serve_sse(stream, &seen).await;
                    }
                    if stall_handshake {
                        return tokio::time::sleep(Duration::from_secs(30)).await;
                    }
                    serve_websocket(stream, script, seen).await;
                });
            }
        });
        Self { base_url, seen }
    }

    fn seen<T>(&self, read: impl FnOnce(&Seen) -> T) -> T {
        read(&self.seen.lock().unwrap())
    }
}

async fn peek_head(stream: &TcpStream) -> String {
    let mut buffer = vec![0u8; 8192];
    for _ in 0..100 {
        let read = stream.peek(&mut buffer).await.unwrap_or_default();
        let head = String::from_utf8_lossy(&buffer[..read]).into_owned();
        if head.contains("\r\n\r\n") {
            return head;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    String::new()
}

async fn serve_sse(mut stream: TcpStream, seen: &Mutex<Seen>) {
    seen.lock().unwrap().http_requests += 1;
    let mut buffer = vec![0u8; 65536];
    let _ = stream.read(&mut buffer).await;
    let body = sse_body(&events("response.completed", "completed", None));
    let head = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(format!("{head}{body}").as_bytes()).await;
}

// tungstenite's handshake callback signature returns a large error type.
#[allow(clippy::result_large_err)]
async fn serve_websocket(stream: TcpStream, script: Script, seen: Arc<Mutex<Seen>>) {
    let captured: Arc<Mutex<HashMap<String, String>>> = Arc::default();
    let headers = captured.clone();
    let callback = move |request: &Request, response: Response| {
        let mut headers = headers.lock().unwrap();
        for (name, value) in request.headers() {
            headers.insert(name.as_str().to_string(), value.to_str().unwrap_or_default().to_string());
        }
        Ok(response)
    };
    let Ok(mut socket) = tokio_tungstenite::accept_hdr_async(stream, callback).await else { return };
    let connection = {
        let mut seen = seen.lock().unwrap();
        seen.connections += 1;
        seen.headers.push(captured.lock().unwrap().clone());
        seen.connections
    };
    while let Some(Ok(message)) = socket.next().await {
        let body: Value = match message {
            WsMessage::Text(text) => serde_json::from_str(&text).unwrap(),
            WsMessage::Close(_) => {
                seen.lock().unwrap().closes += 1;
                return;
            }
            _ => continue,
        };
        seen.lock().unwrap().bodies.push((connection, body.clone()));
        let (events, stall) = match script(connection, &body) {
            Action::Reply(events) => (events, false),
            Action::ReplyThenStall(events) => (events, true),
            Action::Stall => (Vec::new(), true),
        };
        for event in events {
            let _ = socket.send(WsMessage::Text(event.to_string().into())).await;
        }
        if stall {
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    }
}

fn completed(id: &str) -> Vec<Value> {
    let mut events = events("response.done", "completed", Some(false));
    events.last_mut().unwrap()["response"]["id"] = json!(id);
    events
}

fn options(token: &str, transport: Transport, session: Option<&str>) -> StreamOptions {
    StreamOptions {
        api_key: Some(token.into()),
        transport: Some(transport),
        session_id: session.map(str::to_string),
        ..StreamOptions::default()
    }
}

#[tokio::test]
async fn auto_transport_streams_over_a_websocket_with_session_headers() {
    let server = Server::start(Arc::new(|_, _| Action::Reply(completed("resp_1"))), false).await;
    let raw: Arc<Mutex<Vec<String>>> = Arc::default();
    let recorder = raw.clone();
    let token = mock_token("acc_ws");
    let options = StreamOptions {
        on_provider_stream_event: Some(Arc::new(move |event, _| {
            recorder.lock().unwrap().push(event["type"].as_str().unwrap_or_default().to_string());
        })),
        ..options(&token, Transport::Auto, Some("session-auto"))
    };
    let (events, message) =
        collect(OpenAICodexResponses.stream(&codex_model(&server.base_url), &context(), options)).await;
    assert_eq!(kinds(&events).iter().filter(|kind| *kind == "start").count(), 1);
    assert_eq!(
        (message.text(), message.stop_reason, message.end_turn),
        ("Hello".to_string(), StopReason::Stop, Some(false))
    );
    assert_eq!(raw.lock().unwrap().last().map(String::as_str), Some("response.done"));
    let headers = server.seen(|seen| seen.headers[0].clone());
    assert_eq!(
        (headers["session-id"].as_str(), headers["x-client-request-id"].as_str()),
        ("session-auto", "session-auto")
    );
    assert_eq!(headers["openai-beta"], "responses_websockets=2026-02-06");
    assert_eq!(headers["chatgpt-account-id"], "acc_ws");
    assert!(!headers.contains_key("session_id"));
    let body = server.seen(|seen| seen.bodies[0].1.clone());
    assert_eq!((body["type"].as_str(), body["store"].as_bool()), (Some("response.create"), Some(false)));
    assert_eq!(server.seen(|seen| seen.http_requests), 0);
    let stats = websocket_debug_stats("session-auto").unwrap();
    assert_eq!((stats.cached_context_requests, stats.full_context_requests), (1, 1));
}

#[tokio::test]
async fn websocket_cached_mode_sends_only_input_deltas_on_a_reused_socket() {
    let server = Server::start(
        Arc::new(|_, body| {
            let id = if body.get("previous_response_id").is_some() { "resp_2" } else { "resp_1" };
            Action::Reply(completed(id))
        }),
        false,
    )
    .await;
    let token = mock_token("acc_delta");
    let model = codex_model(&server.base_url);
    let first = OpenAICodexResponses
        .stream(&model, &context(), options(&token, Transport::WebsocketCached, Some("session-delta")))
        .result(&model)
        .await;
    assert_eq!(first.response_id.as_deref(), Some("resp_1"));
    let mut next = context();
    next.messages.push(Message::Assistant(first));
    next.messages.push(Message::user("Now finish"));
    let second = OpenAICodexResponses
        .stream(&model, &next, options(&token, Transport::WebsocketCached, Some("session-delta")))
        .result(&model)
        .await;
    assert_eq!(second.stop_reason, StopReason::Stop, "{:?}", second.error_message);
    let bodies = server.seen(|seen| seen.bodies.clone());
    assert_eq!((bodies[0].0, bodies[1].0), (1, 1));
    assert!(bodies[0].1.get("previous_response_id").is_none());
    assert_eq!(bodies[1].1["previous_response_id"], "resp_1");
    assert_eq!(
        bodies[1].1["input"],
        json!([{"role": "user", "content": [{"type": "input_text", "text": "Now finish"}]}])
    );
    let stats = websocket_debug_stats("session-delta").unwrap();
    assert_eq!((stats.connections_created, stats.connections_reused, stats.delta_requests), (1, 1, 1));
    assert_eq!(stats.last_previous_response_id.as_deref(), Some("resp_1"));
}

#[tokio::test]
async fn reconnects_once_when_the_connection_limit_is_reached() {
    let server = Server::start(
        Arc::new(|connection, _| match connection {
            1 => Action::Reply(vec![json!({"type": "error", "error": {"code": "websocket_connection_limit_reached"}})]),
            _ => Action::Reply(completed("resp_1")),
        }),
        false,
    )
    .await;
    let model = codex_model(&server.base_url);
    let message = OpenAICodexResponses
        .stream(&model, &context(), options(&mock_token("acc"), Transport::Auto, None))
        .result(&model)
        .await;
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    assert_eq!(server.seen(|seen| (seen.connections, seen.http_requests)), (2, 0));
}

#[tokio::test]
async fn falls_back_to_sse_after_a_connect_timeout_and_stays_on_sse() {
    let server = Server::start(Arc::new(|_, _| Action::Stall), true).await;
    let model = codex_model(&server.base_url);
    let options = StreamOptions {
        websocket_connect_timeout_ms: Some(50),
        timeout_ms: Some(300_000),
        ..options(&mock_token("acc"), Transport::Auto, Some("ws-connect-timeout"))
    };
    let message = OpenAICodexResponses.stream(&model, &context(), options.clone()).result(&model).await;
    assert_eq!(message.text(), "Hello", "{:?}", message.error_message);
    assert_eq!(message.diagnostics[0]["type"], "provider_transport_failure");
    assert_eq!(message.diagnostics[0]["details"]["fallbackTransport"], "sse");
    let stats = websocket_debug_stats("ws-connect-timeout").unwrap();
    assert_eq!((stats.websocket_failures, stats.sse_fallbacks, stats.websocket_fallback_active), (1, 1, Some(true)));
    assert_eq!(stats.last_websocket_error.as_deref(), Some("WebSocket connect timeout after 50ms"));
    let again = OpenAICodexResponses.stream(&model, &context(), options).result(&model).await;
    assert_eq!(again.text(), "Hello");
    assert_eq!(server.seen(|seen| seen.http_requests), 2);
    assert_eq!(websocket_debug_stats("ws-connect-timeout").unwrap().sse_fallbacks, 2);
}

#[tokio::test]
async fn idle_websockets_fall_back_before_output_and_fail_after_it() {
    let server = Server::start(Arc::new(|_, _| Action::Stall), false).await;
    let model = codex_model(&server.base_url);
    let quiet = StreamOptions { timeout_ms: Some(100), ..options(&mock_token("acc"), Transport::Auto, None) };
    let message = OpenAICodexResponses.stream(&model, &context(), quiet).result(&model).await;
    assert_eq!(message.text(), "Hello", "{:?}", message.error_message);
    assert_eq!(server.seen(|seen| seen.http_requests), 1);

    let partial = events("response.completed", "completed", None)[..3].to_vec();
    let server = Server::start(Arc::new(move |_, _| Action::ReplyThenStall(partial.clone())), false).await;
    let model = codex_model(&server.base_url);
    let quiet = StreamOptions { timeout_ms: Some(100), ..options(&mock_token("acc"), Transport::Auto, None) };
    let message = OpenAICodexResponses.stream(&model, &context(), quiet).result(&model).await;
    assert_eq!(message.error_message.as_deref(), Some("WebSocket idle timeout after 100ms"));
    assert_eq!(message.diagnostics[0]["details"]["phase"], "after_message_stream_start");
    assert_eq!(server.seen(|seen| seen.http_requests), 0);
}

#[tokio::test]
async fn cached_sockets_respect_the_age_limit_and_the_account() {
    let server =
        Server::start(Arc::new(|connection, _| Action::Reply(completed(&format!("resp_{connection}")))), false).await;
    let model = codex_model(&server.base_url);
    let run = |token: String| {
        let model = model.clone();
        async move {
            let options = options(&token, Transport::WebsocketCached, Some("aged-session"));
            OpenAICodexResponses.stream(&model, &context(), options).result(&model).await
        }
    };
    run(mock_token("acc_a")).await;
    websocket::age_cached_socket("aged-session", Duration::from_secs(56 * 60));
    run(mock_token("acc_a")).await;
    assert_eq!(server.seen(|seen| seen.connections), 2);
    run(mock_token("acc_b")).await;
    assert_eq!(server.seen(|seen| seen.connections), 3);
    run(mock_token("acc_b")).await;
    assert_eq!(server.seen(|seen| seen.connections), 3);
    let stats = websocket_debug_stats("aged-session").unwrap();
    assert_eq!((stats.connections_created, stats.connections_reused), (3, 1));
    close_websocket_sessions(Some("aged-session"));
}

#[tokio::test]
async fn one_shot_sockets_close_and_aborts_release_the_session() {
    let server = Server::start(Arc::new(|_, _| Action::Reply(completed("resp_1"))), false).await;
    let model = codex_model(&server.base_url);
    let one_shot = StreamOptions {
        cache_retention: Some(crate::registry::CacheRetention::None),
        ..options(&mock_token("acc"), Transport::Websocket, Some("ignored"))
    };
    let message = OpenAICodexResponses.stream(&model, &context(), one_shot).result(&model).await;
    assert_eq!(message.stop_reason, StopReason::Stop);
    for _ in 0..100 {
        if server.seen(|seen| seen.closes) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(server.seen(|seen| seen.closes), 1);

    let partial = events("response.completed", "completed", None)[..3].to_vec();
    let server = Server::start(
        Arc::new(move |connection, _| match connection {
            1 => Action::ReplyThenStall(partial.clone()),
            _ => Action::Reply(completed("resp_2")),
        }),
        false,
    )
    .await;
    let model = codex_model(&server.base_url);
    let aborting = options(&mock_token("acc"), Transport::WebsocketCached, Some("abort-session"));
    let cancel = aborting.cancel.clone();
    let message = cancel_after_first_text(OpenAICodexResponses.stream(&model, &context(), aborting), cancel).await;
    assert_eq!(message.stop_reason, StopReason::Aborted);
    let options = options(&mock_token("acc"), Transport::WebsocketCached, Some("abort-session"));
    let message = OpenAICodexResponses.stream(&model, &context(), options).result(&model).await;
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    assert_eq!(server.seen(|seen| seen.connections), 2);
}

#[tokio::test]
async fn an_unset_transport_uses_websockets_without_cached_context() {
    let server =
        Server::start(Arc::new(|connection, _| Action::Reply(completed(&format!("resp_{connection}")))), false).await;
    let model = codex_model(&server.base_url);
    let token = mock_token("acc");
    for _ in 0..2 {
        let options = StreamOptions { transport: None, ..options(&token, Transport::Auto, Some("default-transport")) };
        let message = OpenAICodexResponses.stream(&model, &context(), options).result(&model).await;
        assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    }
    assert!(server.seen(|seen| seen.bodies.iter().all(|(_, body)| body.get("previous_response_id").is_none())));
    let stats = websocket_debug_stats("default-transport").unwrap();
    assert_eq!((stats.connections_reused, stats.cached_context_requests, stats.full_context_requests), (1, 0, 2));
    close_websocket_sessions(Some("default-transport"));
}
