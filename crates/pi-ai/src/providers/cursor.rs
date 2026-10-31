//! Cursor's agent service (`api2.cursor.sh`) with a Cursor login, over HTTP/1.1: `RunSSE`
//! streams the run's Connect frames, and each client message is one unary `BidiAppend`.

mod history;
pub(crate) mod proto;
mod request;
mod session;
#[cfg(test)]
mod tests;
mod value;

use std::time::Duration;

use base64::Engine;
use futures::StreamExt;
use reqwest::header::HeaderMap;
use serde_json::Value as Json;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use self::proto::{EnvelopeReader, FLAG_COMPRESSED, FLAG_END_STREAM, Writer, envelope, field, fields};
use self::session::Session;
use crate::auth::oauth::random_uuid;
use crate::providers::common::header_map;
use crate::registry::{ApiProvider, ProviderResponse, StreamOptions};
use crate::simple_options::supports_fast;
use crate::stream::{AssistantMessageEventStream, MessageBuilder, error_stream, event_channel};
use crate::types::{Context, Model, StopReason};

pub const CURSOR_BASE_URL: &str = "https://api2.cursor.sh";
/// The Cursor CLI build named in requests; the service gates protocol features on it.
pub const CURSOR_CLIENT_VERSION: &str = "cli-2026.09.02-c22c1a3";
const ALLOWED_TOOLS: &str = "mcp_tool_call,get_mcp_tools_tool_call";
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const APPEND_TIMEOUT: Duration = Duration::from_secs(60);
/// `AgentClientMessage{client_heartbeat: {}}`.
const HEARTBEAT: [u8; 2] = [0x3a, 0x00];

pub struct CursorAgent;

impl ApiProvider for CursorAgent {
    fn stream(&self, model: &Model, context: &Context, options: StreamOptions) -> AssistantMessageEventStream {
        let Some(token) = options.api_key.clone().filter(|key| !key.is_empty()) else {
            return error_stream(model, StopReason::Error, format!("No API key for provider: {}", model.provider));
        };
        let (sender, stream) = event_channel();
        let (model, context) = (model.clone(), context.clone());
        tokio::spawn(async move {
            let mut out = MessageBuilder::new(&model, sender);
            let result = tokio::select! {
                biased;
                _ = options.cancel.cancelled() => Err(String::new()),
                result = run(&mut out, &model, &context, &options, &token) => result,
            };
            match result {
                _ if options.cancel.is_cancelled() => out.fail(StopReason::Aborted, "Request was aborted"),
                Ok(reason) => out.finish(reason),
                Err(error) => out.fail(StopReason::Error, error),
            }
        });
        stream
    }
}

fn base_url(model: &Model) -> String {
    match model.base_url.trim().trim_end_matches('/') {
        "" => CURSOR_BASE_URL.to_string(),
        base => base.to_string(),
    }
}

/// The headers the Cursor CLI sends with every agent service call.
pub(crate) fn cursor_headers(token: &str, request_id: &str, content_type: &str) -> Vec<(&'static str, String)> {
    vec![
        ("content-type", content_type.to_string()),
        ("authorization", format!("Bearer {token}")),
        ("x-cursor-client-type", "cli".into()),
        ("x-cursor-client-version", CURSOR_CLIENT_VERSION.into()),
        ("x-ghost-mode", "true".into()),
        ("connect-protocol-version", "1".into()),
        ("x-request-id", request_id.to_string()),
    ]
}

fn headers(model: &Model, options: &StreamOptions, token: &str, request_id: &str, content_type: &str) -> HeaderMap {
    let mut defaults = cursor_headers(token, request_id, content_type);
    defaults.push(("x-cursor-streaming", "true".into()));
    defaults.push(("x-cursor-agent-allowed-tools", ALLOWED_TOOLS.into()));
    header_map(defaults, model, options)
}

/// Aborts the append task when the run ends; appends after the end are not read.
struct Appender {
    sender: mpsc::UnboundedSender<Vec<u8>>,
    failures: mpsc::UnboundedReceiver<String>,
    task: JoinHandle<()>,
}

impl Drop for Appender {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Sends `AgentClientMessage`s in order as `BidiAppendRequest{request_id, seqno, data}`.
fn spawn_appender(url: String, headers: HeaderMap, request_id: String) -> Appender {
    let (sender, mut messages) = mpsc::unbounded_channel::<Vec<u8>>();
    let (report, failures) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        let client = crate::registry::http_client();
        let mut seqno = 0u64;
        while let Some(data) = messages.recv().await {
            let mut body = Writer::new().message(2, Writer::new().string(1, &request_id));
            if seqno > 0 {
                body = body.varint(3, seqno);
            }
            seqno += 1;
            let request = client.post(&url).headers(headers.clone()).timeout(APPEND_TIMEOUT);
            let failure = match request.body(body.bytes(4, &data).finish()).send().await {
                Ok(response) if response.status().is_success() => continue,
                Ok(response) => {
                    let status = response.status().as_u16();
                    http_error("BidiAppend", status, &response.text().await.unwrap_or_default())
                }
                Err(error) => format!("Cursor BidiAppend failed: {error}"),
            };
            let _ = report.send(failure);
            return;
        }
    });
    Appender { sender, failures, task }
}

async fn run(
    out: &mut MessageBuilder,
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    token: &str,
) -> Result<StopReason, String> {
    let request_id = random_uuid();
    let fast = options.fast && supports_fast(model);
    let built = request::build(model, context, options.reasoning, fast, &random_uuid(), &random_uuid());
    let base = base_url(model);
    let run_headers = headers(model, options, token, &request_id, "application/connect+proto");
    let open_body = envelope(0, &Writer::new().string(1, &request_id).finish());
    let open = crate::registry::http_client()
        .post(format!("{base}/agent.v1.AgentService/RunSSE"))
        .headers(run_headers)
        .body(open_body)
        .send();
    let open = tokio::spawn(open);
    let append_headers = headers(model, options, token, &request_id, "application/proto");
    let mut appender = spawn_appender(format!("{base}/aiserver.v1.BidiService/BidiAppend"), append_headers, request_id);
    let _ = appender.sender.send(built.message);
    let response =
        open.await.map_err(|error| error.to_string())?.map_err(|error| format!("Cursor request failed: {error}"))?;
    let status = response.status().as_u16();
    if let Some(hook) = &options.on_response {
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_str().unwrap_or_default().to_string()));
        hook(&ProviderResponse { status, headers: headers.collect() }, model);
    }
    if !response.status().is_success() {
        return Err(http_error("RunSSE", status, &response.text().await.unwrap_or_default()));
    }
    out.start();
    let mut session = Session::new(built.blobs, built.request_context, built.tools, model.cost.clone());
    let mut frames = EnvelopeReader::default();
    let mut body = response.bytes_stream();
    let start = tokio::time::Instant::now() + HEARTBEAT_INTERVAL;
    let mut heartbeat = tokio::time::interval_at(start, HEARTBEAT_INTERVAL);
    loop {
        tokio::select! {
            chunk = body.next() => {
                let Some(chunk) = chunk else { break };
                frames.push(&chunk.map_err(|error| format!("Cursor stream failed: {error}"))?);
                while let Some((flags, payload)) = frames.next_envelope() {
                    if flags & FLAG_END_STREAM != 0 {
                        end_stream(&payload)?;
                        return session.finish(out);
                    }
                    if flags & FLAG_COMPRESSED != 0 {
                        return Err("Cursor sent a compressed frame".into());
                    }
                    for reply in session.handle(out, &payload)? {
                        let _ = appender.sender.send(reply);
                    }
                }
            }
            Some(failure) = appender.failures.recv() => return Err(failure),
            _ = heartbeat.tick() => {
                let _ = appender.sender.send(HEARTBEAT.to_vec());
            }
        }
    }
    if frames.has_partial() {
        return Err("Cursor stream ended inside a frame".into());
    }
    session.finish(out)
}

fn http_error(endpoint: &str, status: u16, body: &str) -> String {
    let parsed: Option<Json> = serde_json::from_str(body).ok();
    let detail = match parsed.as_ref().map(|error| (error["code"].as_str(), error["message"].as_str())) {
        Some((Some(code), Some(message))) => format!("{code}: {message}"),
        Some((None, Some(message))) => message.to_string(),
        _ => body.trim().chars().take(500).collect(),
    };
    let hint = if status == 401 { "\nRun /login cursor again." } else { "" };
    format!("Cursor {endpoint} HTTP {status}: {detail}{hint}")
}

/// The error of an end-stream frame, from `aiserver.v1.ErrorDetails` when Cursor sends one.
fn end_stream(payload: &[u8]) -> Result<(), String> {
    let frame: Json = serde_json::from_slice(payload).unwrap_or_default();
    let Some(error) = frame.get("error").filter(|error| error.is_object()) else { return Ok(()) };
    let details = error["details"].as_array().into_iter().flatten();
    let described = details
        .filter(|detail| detail["type"].as_str().is_some_and(|kind| kind.ends_with("aiserver.v1.ErrorDetails")))
        .filter_map(|detail| detail["value"].as_str())
        .find_map(|value| describe_error_details(value.trim_end_matches('=')));
    Err(described.unwrap_or_else(|| {
        let code = error["code"].as_str().unwrap_or("unknown");
        format!("Connect error {code}: {}", error["message"].as_str().unwrap_or_default())
    }))
}

fn describe_error_details(value: &str) -> Option<String> {
    let engines = [base64::engine::general_purpose::STANDARD_NO_PAD, base64::engine::general_purpose::URL_SAFE_NO_PAD];
    let bytes = engines.iter().find_map(|engine| engine.decode(value).ok())?;
    let details = fields(&bytes).ok()?;
    let custom = fields(field(&details, 2)?.as_bytes()?).ok()?;
    let text = |number| field(&custom, number).and_then(proto::Value::as_str).unwrap_or_default();
    let parts: Vec<&str> = [text(1), text(2)].into_iter().filter(|part| !part.is_empty()).collect();
    (!parts.is_empty()).then(|| format!("Cursor error: {}", parts.join(": ")))
}
