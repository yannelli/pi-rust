//! The loopback MCP server that offers pi's tools to the Antigravity agent (streamable HTTP,
//! stateless). A `tools/call` stays open until pi has run the tool and sends the result back.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::auth::oauth::random_uuid;

pub(crate) const BRIDGE_NAME: &str = "pi-bridge";
const PROTOCOL_VERSIONS: [&str; 5] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05", "2024-10-07"];
const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_TOOLS: usize = 64;
const KEEPALIVE: Duration = Duration::from_secs(15);
const SCHEMA_ANNOTATIONS: [&str; 7] = ["$schema", "$id", "$comment", "examples", "deprecated", "readOnly", "writeOnly"];

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BridgeTool {
    /// The pi tool name.
    pub name: String,
    pub description: String,
    pub schema: Value,
}

/// A `tools/call` waiting for pi. `reply` takes the MCP `CallToolResult`.
#[derive(Debug)]
pub(crate) struct ParkedCall {
    pub id: String,
    pub tool: String,
    pub arguments: Map<String, Value>,
    pub reply: oneshot::Sender<Value>,
}

pub(crate) struct Bridge {
    pub url: String,
    pub token: String,
    tools: Arc<RwLock<Vec<BridgeTool>>>,
    task: JoinHandle<()>,
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// `pi_<name>` with characters outside `[A-Za-z0-9_-]` as `_`, at most 64 characters.
pub(crate) fn exposed_name(name: &str) -> String {
    let clean = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' });
    format!("pi_{}", clean.collect::<String>()).chars().take(64).collect()
}

pub(crate) fn error_result(text: &str) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": true})
}

fn clean_schema(schema: &Value) -> Value {
    match schema {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .filter(|(key, _)| !SCHEMA_ANNOTATIONS.contains(&key.as_str()))
                .map(|(key, value)| (key.clone(), clean_schema(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(clean_schema).collect()),
        other => other.clone(),
    }
}

impl Bridge {
    pub async fn start(calls: mpsc::UnboundedSender<ParkedCall>) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/mcp", listener.local_addr()?);
        let token = random_uuid().replace('-', "");
        let tools: Arc<RwLock<Vec<BridgeTool>>> = Arc::default();
        let state = Arc::new((token.clone(), tools.clone(), calls));
        let task = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                tokio::spawn(serve(socket, state.clone()));
            }
        });
        Ok(Self { url, token, tools, task })
    }

    pub fn set_tools(&self, tools: Vec<BridgeTool>) {
        *self.tools.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = tools;
    }
}

type State = Arc<(String, Arc<RwLock<Vec<BridgeTool>>>, mpsc::UnboundedSender<ParkedCall>)>;

async fn serve(mut socket: TcpStream, state: State) {
    let Some((method, path, headers, body)) = read_request(&mut socket).await else { return };
    let header = |name: &str| headers.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str());
    let expected = format!("Bearer {}", state.0);
    let authorized = header("authorization").is_some_and(|value| constant_eq(value.as_bytes(), expected.as_bytes()));
    let status = match (method.as_str(), path.split('?').next(), authorized) {
        (_, _, false) => Some("401 Unauthorized"),
        ("POST", Some("/mcp"), true) => None,
        _ => Some("405 Method Not Allowed"),
    };
    if let Some(status) = status {
        let _ = socket
            .write_all(format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").as_bytes())
            .await;
        return;
    }
    let messages = match serde_json::from_slice::<Value>(&body) {
        Ok(Value::Array(batch)) => batch,
        Ok(message @ Value::Object(_)) => vec![message],
        _ => {
            let _ =
                socket.write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
            return;
        }
    };
    let requests: Vec<Value> =
        messages.into_iter().filter(|message| message.get("id").is_some_and(|id| !id.is_null())).collect();
    if requests.is_empty() {
        let _ = socket.write_all(b"HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
        return;
    }
    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache, no-transform\r\nconnection: close\r\n\r\n";
    if socket.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    for request in requests {
        let reply = respond(&mut socket, &state, &request).await;
        let frame =
            format!("event: message\ndata: {}\n\n", json!({"jsonrpc": "2.0", "id": request["id"], "result": reply}));
        if socket.write_all(frame.as_bytes()).await.is_err() {
            return;
        }
    }
}

async fn respond(socket: &mut TcpStream, state: &State, request: &Value) -> Value {
    let params = &request["params"];
    match request["method"].as_str().unwrap_or_default() {
        "initialize" => {
            let asked = params["protocolVersion"].as_str().filter(|version| PROTOCOL_VERSIONS.contains(version));
            json!({"protocolVersion": asked.unwrap_or(PROTOCOL_VERSIONS[0]), "capabilities": {"tools": {}},
                "serverInfo": {"name": "pi-antigravity-tools", "version": env!("CARGO_PKG_VERSION")}})
        }
        "tools/list" => {
            let tools = state.1.read().unwrap_or_else(|poisoned| poisoned.into_inner());
            let listed = tools.iter().take(MAX_TOOLS).map(|tool| {
                json!({"name": exposed_name(&tool.name), "description": tool.description, "inputSchema": clean_schema(&tool.schema)})
            });
            json!({"tools": listed.collect::<Vec<_>>()})
        }
        "tools/call" => call(socket, state, params).await,
        _ => json!({}),
    }
}

async fn call(socket: &mut TcpStream, state: &State, params: &Value) -> Value {
    let requested = params["name"].as_str().unwrap_or_default();
    let tool = {
        let tools = state.1.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        tools.iter().find(|tool| exposed_name(&tool.name) == requested).map(|tool| tool.name.clone())
    };
    let Some(tool) = tool else { return error_result(&format!("Unknown pi tool: {requested}")) };
    let arguments = params["arguments"].as_object().cloned().unwrap_or_default();
    let (reply, result) = oneshot::channel();
    if state.2.send(ParkedCall { id: random_uuid(), tool, arguments, reply }).is_err() {
        return error_result("pi is not running this Antigravity session");
    }
    let mut result = std::pin::pin!(result);
    loop {
        tokio::select! {
            outcome = &mut result => return outcome.unwrap_or_else(|_| error_result("pi ended the turn without a tool result")),
            () = tokio::time::sleep(KEEPALIVE) => {
                if socket.write_all(b": keepalive\n\n").await.is_err() {
                    return error_result("connection closed");
                }
            }
        }
    }
}

fn constant_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len() && left.iter().zip(right).fold(0u8, |diff, (a, b)| diff | (a ^ b)) == 0
}

async fn read_request(socket: &mut TcpStream) -> Option<(String, String, Vec<(String, String)>, Vec<u8>)> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
        if buffer.len() > 64 * 1024 {
            return None;
        }
        let read = socket.read(&mut chunk).await.ok().filter(|read| *read > 0)?;
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut start = head.lines().next()?.split_whitespace();
    let (method, path) = (start.next()?.to_string(), start.next()?.to_string());
    let headers: Vec<(String, String)> = head
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    let length = headers.iter().find(|(name, _)| name == "content-length").and_then(|(_, value)| value.parse().ok());
    let length: usize = length.unwrap_or(0);
    if length > MAX_BODY_BYTES {
        return None;
    }
    while buffer.len() < head_end + length {
        let read = socket.read(&mut chunk).await.ok().filter(|read| *read > 0)?;
        buffer.extend_from_slice(&chunk[..read]);
    }
    Some((method, path, headers, buffer[head_end..head_end + length].to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn post(bridge: &Bridge, token: &str, body: Value) -> (u16, String) {
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let response = client.post(&bridge.url).bearer_auth(token).json(&body).send().await.unwrap();
        (response.status().as_u16(), response.text().await.unwrap())
    }

    #[tokio::test]
    async fn lists_tools_and_parks_calls_until_pi_answers() {
        let (calls, mut parked) = mpsc::unbounded_channel();
        let bridge = Bridge::start(calls).await.unwrap();
        let schema = json!({"$schema": "x", "type": "object", "properties": {"path": {"type": "string"}}});
        bridge.set_tools(vec![BridgeTool { name: "read".into(), description: "Read".into(), schema }]);
        assert_eq!(post(&bridge, "wrong", json!({})).await.0, 401);
        let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert_eq!(post(&bridge, &bridge.token, note).await.0, 202);
        let (status, body) =
            post(&bridge, &bridge.token, json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})).await;
        assert_eq!(status, 200);
        assert!(body.contains(r#""name":"pi_read""#) && !body.contains("$schema"), "{body}");
        let token = bridge.token.clone();
        let answer = tokio::spawn(async move {
            let call = parked.recv().await.unwrap();
            assert_eq!((call.tool.as_str(), &call.arguments["path"]), ("read", &json!("a")));
            call.reply.send(json!({"content": [{"type": "text", "text": "body"}], "isError": false})).unwrap();
        });
        let request = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "pi_read", "arguments": {"path": "a"}}});
        let (_, body) = post(&bridge, &token, request).await;
        answer.await.unwrap();
        assert!(body.starts_with("event: message\ndata: ") && body.contains(r#""text":"body""#), "{body}");
    }
}
