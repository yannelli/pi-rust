use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::proto::{FLAG_END_STREAM, Value, Writer, envelope, field, fields};
use super::session::HANDOFF_TEXT;
use super::{CURSOR_CLIENT_VERSION, CursorAgent};
use crate::providers::mock_server::{collect, kinds, test_model};
use crate::registry::{ApiProvider, StreamOptions};
use crate::types::{ContentBlock, Context, Message, StopReason, Tool};

enum Step {
    Frame(Vec<u8>),
    /// Wait until this many `BidiAppend`s arrived.
    Appends(usize),
}

#[derive(Default)]
struct Seen {
    appends: Vec<(u64, Vec<u8>)>,
    headers: HashMap<String, HashMap<String, String>>,
}

struct CursorMock {
    base_url: String,
    seen: Arc<Mutex<Seen>>,
}

impl CursorMock {
    async fn start(script: Vec<Step>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Seen::default()));
        let script = Arc::new(Mutex::new(Some(script)));
        let state = seen.clone();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                tokio::spawn(serve(socket, state.clone(), script.clone()));
            }
        });
        Self { base_url, seen }
    }

    /// The `AgentClientMessage` of every append, in seqno order.
    fn messages(&self) -> Vec<Vec<u8>> {
        let mut appends = self.seen.lock().unwrap().appends.clone();
        appends.sort_by_key(|(seqno, _)| *seqno);
        appends.into_iter().map(|(_, data)| data).collect()
    }
}

async fn serve(mut socket: TcpStream, seen: Arc<Mutex<Seen>>, script: Arc<Mutex<Option<Vec<Step>>>>) {
    while let Some((path, headers, body)) = read_request(&mut socket).await {
        let name = path.rsplit('/').next().unwrap_or_default().to_string();
        seen.lock().unwrap().headers.insert(name.clone(), headers);
        if name == "BidiAppend" {
            let request = fields(&body).unwrap();
            let seqno = field(&request, 3).and_then(Value::as_u64).unwrap_or(0);
            let data = field(&request, 4).and_then(Value::as_bytes).unwrap_or_default().to_vec();
            seen.lock().unwrap().appends.push((seqno, data));
            socket.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n").await.unwrap();
            continue;
        }
        let head = "HTTP/1.1 200 OK\r\ncontent-type: application/connect+proto\r\nconnection: close\r\n\r\n";
        socket.write_all(head.as_bytes()).await.unwrap();
        let steps = script.lock().unwrap().take().unwrap_or_default();
        for step in steps {
            match step {
                Step::Frame(frame) => socket.write_all(&frame).await.unwrap(),
                Step::Appends(count) => {
                    let wait = async {
                        while seen.lock().unwrap().appends.len() < count {
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                    };
                    tokio::time::timeout(Duration::from_secs(5), wait).await.expect("client appends");
                }
            }
        }
        return;
    }
}

async fn read_request(socket: &mut TcpStream) -> Option<(String, HashMap<String, String>, Vec<u8>)> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
        let read = socket.read(&mut chunk).await.ok().filter(|read| *read > 0)?;
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let path = head.split_whitespace().nth(1)?.to_string();
    let headers: HashMap<String, String> = head
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    let length: usize = headers.get("content-length").and_then(|value| value.parse().ok()).unwrap_or(0);
    while buffer.len() < head_end + length {
        let read = socket.read(&mut chunk).await.ok().filter(|read| *read > 0)?;
        buffer.extend_from_slice(&chunk[..read]);
    }
    Some((path, headers, buffer[head_end..head_end + length].to_vec()))
}

fn server(number: u32, body: Writer) -> Step {
    Step::Frame(envelope(0, &Writer::new().message(number, body).finish()))
}

fn update(number: u32, body: Writer) -> Step {
    server(1, Writer::new().message(number, body))
}

fn turn_ended(input: u64, output: u64) -> Step {
    update(14, Writer::new().varint(1, input).varint(2, output))
}

fn end(json: &str) -> Step {
    Step::Frame(envelope(FLAG_END_STREAM, json.as_bytes()))
}

fn read_args(path: Option<&str>) -> Writer {
    let mut args = Writer::new().string(1, "read").string(3, "call_1").string(5, "read");
    if let Some(path) = path {
        args = args.message(2, Writer::new().string(1, "path").message(2, Writer::new().string(3, path)));
    }
    args
}

fn exec(number: u32, body: Writer) -> Step {
    server(2, Writer::new().varint(1, 7).string(15, "exec-1").message(number, body))
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

fn options() -> StreamOptions {
    StreamOptions { api_key: Some("cursor-token".into()), ..StreamOptions::default() }
}

fn context() -> Context {
    let tool = Tool::new("read", "Read a file", json!({"type": "object", "properties": {"path": {"type": "string"}}}));
    Context { system_prompt: Some("Be brief".into()), messages: vec![Message::user("hi")], tools: vec![tool] }
}

#[tokio::test]
async fn streams_text_and_thinking_and_serves_blobs() {
    let system_record = json!({"role": "system", "content": "Be brief"}).to_string();
    let blob_id = ring::digest::digest(&ring::digest::SHA256, system_record.as_bytes());
    let server = CursorMock::start(vec![
        Step::Appends(1),
        server(4, Writer::new().varint(1, 3).message(2, Writer::new().bytes(1, blob_id.as_ref()))),
        Step::Appends(2),
        update(4, Writer::new().string(1, "Think")),
        update(5, Writer::new()),
        update(1, Writer::new().string(1, "Hel")),
        update(1, Writer::new().string(1, "lo")),
        turn_ended(120, 8),
        server(3, Writer::new().message(5, Writer::new().varint(1, 4321).varint(2, 300_000))),
        end("{}"),
    ])
    .await;
    let model = test_model("cursor-agent", "cursor", &server.base_url);
    let (events, message) = collect(CursorAgent.stream(&model, &context(), options())).await;
    assert_eq!(message.error_message, None);
    assert_eq!(
        kinds(&events),
        [
            "start",
            "thinking_start",
            "thinking_delta",
            "thinking_end",
            "text_start",
            "text_delta",
            "text_delta",
            "text_end",
            "done"
        ]
    );
    assert_eq!((message.stop_reason, message.usage.input, message.usage.output), (StopReason::Stop, 120, 8));
    assert_eq!(message.usage.total_tokens, 4321);
    let messages = server.messages();
    let run = fields(&messages[0]).unwrap();
    let run = fields(field(&run, 1).unwrap().as_bytes().unwrap()).unwrap();
    assert!(field(&run, 1).is_some() && field(&run, 9).is_some());
    let kv = fields(field(&fields(&messages[1]).unwrap(), 3).unwrap().as_bytes().unwrap()).unwrap();
    assert_eq!(field(&kv, 1).and_then(Value::as_u64), Some(3));
    let result = fields(field(&kv, 2).unwrap().as_bytes().unwrap()).unwrap();
    assert_eq!(field(&result, 1).and_then(Value::as_bytes), Some(system_record.as_bytes()));
    let seen = server.seen.lock().unwrap();
    assert_eq!(seen.headers["RunSSE"]["content-type"], "application/connect+proto");
    assert_eq!(seen.headers["RunSSE"]["authorization"], "Bearer cursor-token");
    assert_eq!(seen.headers["BidiAppend"]["content-type"], "application/proto");
    assert_eq!(seen.headers["BidiAppend"]["x-cursor-client-version"], CURSOR_CLIENT_VERSION);
}

#[tokio::test]
async fn mcp_tool_calls_are_handed_off_to_pi() {
    let tool_call = |args: Writer| Writer::new().message(15, Writer::new().message(1, args));
    let server = CursorMock::start(vec![
        Step::Appends(1),
        exec(10, Writer::new()),
        Step::Appends(2),
        update(2, Writer::new().string(1, "c1").message(2, tool_call(read_args(None)))),
        update(7, Writer::new().string(1, "c1").message(2, tool_call(read_args(None))).string(3, r#"{"path":"#)),
        update(7, Writer::new().string(1, "c1").message(2, tool_call(read_args(None))).string(3, r#"{"path":"a"}"#)),
        update(3, Writer::new().string(1, "c1").message(2, tool_call(read_args(Some("a"))))),
        exec(11, read_args(Some("a"))),
        Step::Appends(3),
        turn_ended(50, 5),
        end("{}"),
    ])
    .await;
    let model = test_model("cursor-agent", "cursor", &server.base_url);
    let (events, message) = collect(CursorAgent.stream(&model, &context(), options())).await;
    assert_eq!(message.error_message, None);
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert!(kinds(&events).contains(&"toolcall_delta".to_string()));
    let ContentBlock::ToolCall(call) = &message.content[0] else { panic!("tool call") };
    assert_eq!((call.id.as_str(), call.name.as_str()), ("call_1", "read"));
    assert_eq!(call.arguments["path"], "a");
    let messages = server.messages();
    assert!(contains(&messages[1], b"Read a file") && contains(&messages[1], b"/pi/system-prompt/0.mdc"));
    assert!(contains(&messages[2], HANDOFF_TEXT.as_bytes()));
}

#[tokio::test]
async fn end_stream_errors_and_cut_streams_fail() {
    let details = Writer::new().varint(1, 8).message(2, Writer::new().string(1, "Rate limited").string(2, "Try later"));
    let value = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, details.finish());
    let error = json!({"error": {"code": "resource_exhausted", "message": "x",
        "details": [{"type": "aiserver.v1.ErrorDetails", "value": value}]}});
    let server = CursorMock::start(vec![Step::Appends(1), end(&error.to_string())]).await;
    let model = test_model("cursor-agent", "cursor", &server.base_url);
    let (_, message) = collect(CursorAgent.stream(&model, &context(), options())).await;
    assert_eq!(message.error_message.as_deref(), Some("Cursor error: Rate limited: Try later"));

    let server = CursorMock::start(vec![Step::Appends(1), update(1, Writer::new().string(1, "partial"))]).await;
    let model = test_model("cursor-agent", "cursor", &server.base_url);
    let (_, message) = collect(CursorAgent.stream(&model, &context(), options())).await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(message.error_message.as_deref(), Some("Cursor stream ended before the turn ended"));
}

#[tokio::test]
async fn an_exec_before_its_updates_opens_one_tool_call() {
    let tool_call = |args: Writer| Writer::new().message(15, Writer::new().message(1, args));
    let server = CursorMock::start(vec![
        Step::Appends(1),
        exec(11, read_args(Some("a"))),
        update(2, Writer::new().string(1, "c1").message(2, tool_call(read_args(None)))),
        update(3, Writer::new().string(1, "c1").message(2, tool_call(read_args(Some("a"))))),
        Step::Appends(2),
        turn_ended(10, 1),
        end("{}"),
    ])
    .await;
    let model = test_model("cursor-agent", "cursor", &server.base_url);
    let (_, message) = collect(CursorAgent.stream(&model, &context(), options())).await;
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    let calls: Vec<&ContentBlock> =
        message.content.iter().filter(|block| matches!(block, ContentBlock::ToolCall(_))).collect();
    assert_eq!(calls.len(), 1);
}
