//! JSON-RPC 2.0 over newline-delimited JSON, the ACP stdio framing. Replies resolve their request;
//! notifications and agent requests arrive on [`Connection::incoming`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;

/// A notification or a request from the agent. `id` is set for requests, which need a reply.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Incoming {
    pub id: Option<Value>,
    pub method: String,
    pub params: Value,
}

#[derive(Debug)]
pub(crate) struct RpcError {
    pub code: i64,
    pub message: String,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} ({})", self.message, self.code)
    }
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>>;

pub(crate) struct Connection {
    label: &'static str,
    writer: mpsc::UnboundedSender<String>,
    pending: Pending,
    next_id: AtomicU64,
    pub incoming: tokio::sync::Mutex<mpsc::UnboundedReceiver<Incoming>>,
    closed: Arc<Mutex<Option<String>>>,
}

impl Connection {
    /// `label` names the agent in errors, for example "Antigravity ACP".
    pub fn new<R, W>(label: &'static str, reader: R, writer: W) -> Arc<Self>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (out, mut lines) = mpsc::unbounded_channel::<String>();
        let (events, incoming) = mpsc::unbounded_channel();
        let pending: Pending = Arc::default();
        let closed: Arc<Mutex<Option<String>>> = Arc::default();
        tokio::spawn(async move {
            let mut writer = writer;
            while let Some(line) = lines.recv().await {
                if writer.write_all(line.as_bytes()).await.is_err() || writer.write_all(b"\n").await.is_err() {
                    break;
                }
                let _ = writer.flush().await;
            }
        });
        let (reader_pending, reader_closed) = (pending.clone(), closed.clone());
        tokio::spawn(async move {
            let reason = read_loop(label, reader, &reader_pending, &events).await;
            *reader_closed.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(reason.clone());
            let senders: Vec<_> =
                reader_pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).drain().collect();
            for (_, sender) in senders {
                let _ = sender.send(Err(RpcError { code: -32099, message: reason.clone() }));
            }
        });
        Arc::new(Self {
            label,
            writer: out,
            pending,
            next_id: AtomicU64::new(0),
            incoming: tokio::sync::Mutex::new(incoming),
            closed,
        })
    }

    fn send(&self, message: &Value) {
        let _ = self.writer.send(message.to_string());
    }

    pub fn closed_reason(&self) -> Option<String> {
        self.closed.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    pub async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(id, sender);
        if let Some(reason) = self.closed_reason() {
            self.pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(&id);
            return Err(RpcError { code: -32099, message: reason });
        }
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(RpcError { code: -32099, message: format!("{} connection closed", self.label) }),
            Err(_) => {
                self.pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(&id);
                Err(RpcError { code: -32098, message: format!("{} {method} timed out", self.label) })
            }
        }
    }

    pub fn notify(&self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    pub fn respond(&self, id: Value, result: Value) {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    pub fn respond_error(&self, id: Value, code: i64, message: &str) {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}));
    }
}

async fn read_loop<R: AsyncRead + Unpin>(
    label: &str,
    reader: R,
    pending: &Pending,
    events: &mpsc::UnboundedSender<Incoming>,
) -> String {
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    loop {
        line.clear();
        let mut limited = (&mut reader).take(MAX_FRAME_BYTES as u64 + 1);
        match limited.read_until(b'\n', &mut line).await {
            Ok(0) => return format!("{label} exited"),
            Ok(_) if line.len() > MAX_FRAME_BYTES => return format!("{label} frame exceeds 32 MiB"),
            Ok(_) => {}
            Err(error) => return format!("{label} read failed: {error}"),
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end_matches(['\n', '\r']).trim();
        // Agents and their tools print log lines to stdout, for example Chromium during a login.
        if !text.starts_with('{') {
            continue;
        }
        let Ok(Value::Object(message)) = serde_json::from_str::<Value>(text) else {
            return format!("{label} sent a line that is not JSON-RPC: {}", text.chars().take(200).collect::<String>());
        };
        let id = message.get("id").cloned().filter(|id| !id.is_null());
        match (message.get("method").and_then(Value::as_str), id) {
            (Some(method), id) => {
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                let _ = events.send(Incoming { id, method: method.to_string(), params });
            }
            (None, Some(id)) => {
                let Some(sender) =
                    id.as_u64().and_then(|id| pending.lock().unwrap_or_else(|p| p.into_inner()).remove(&id))
                else {
                    continue;
                };
                let result = match message.get("error") {
                    Some(error) => Err(RpcError {
                        code: error["code"].as_i64().unwrap_or(-32603),
                        message: error["message"].as_str().unwrap_or("ACP agent error").to_string(),
                    }),
                    None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = sender.send(result);
            }
            (None, None) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn requests_resolve_and_agent_messages_arrive() {
        let (client, agent) = tokio::io::duplex(1 << 16);
        let (client_read, client_write) = tokio::io::split(client);
        let connection = Connection::new("Test ACP", client_read, client_write);
        let (agent_read, mut agent_write) = tokio::io::split(agent);
        tokio::spawn(async move {
            let mut lines = BufReader::new(agent_read).lines();
            let request: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(request["method"], "initialize");
            let notice = "Opening in existing browser session.\n";
            let update = json!({"jsonrpc": "2.0", "method": "session/update", "params": {"x": 1}});
            let reply = json!({"jsonrpc": "2.0", "id": request["id"], "result": {"protocolVersion": 1}});
            agent_write.write_all(format!("{notice}{update}\n{reply}\n").as_bytes()).await.unwrap();
        });
        let result = connection.request("initialize", json!({}), Duration::from_secs(5)).await.unwrap();
        assert_eq!(result["protocolVersion"], 1);
        let event = connection.incoming.lock().await.recv().await.unwrap();
        assert_eq!((event.method.as_str(), event.id), ("session/update", None));
    }
}
