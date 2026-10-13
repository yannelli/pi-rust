//! A scripted ACP client: newline-delimited JSON-RPC requests, notifications, and responses.

use std::io::Write;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// The agent's output: complete lines go to the client.
pub struct ChannelWriter {
    sender: UnboundedSender<String>,
    buffer: Vec<u8>,
}

impl ChannelWriter {
    pub fn new() -> (Self, UnboundedReceiver<String>) {
        let (sender, receiver) = unbounded_channel();
        (Self { sender, buffer: Vec::new() }, receiver)
    }
}

impl Write for ChannelWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.buffer.extend_from_slice(bytes);
        while let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=end).collect();
            let _ = self.sender.send(String::from_utf8_lossy(&line).trim_end().to_string());
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub struct Client<W> {
    input: W,
    lines: UnboundedReceiver<String>,
    next_id: i64,
    /// Every record received so far, in order.
    pub seen: Vec<Value>,
    cursor: usize,
}

impl<W: AsyncWrite + Unpin + Send> Client<W> {
    pub fn new(input: W, lines: UnboundedReceiver<String>) -> Self {
        Self { input, lines, next_id: 0, seen: Vec::new(), cursor: 0 }
    }

    async fn write(&mut self, record: Value) {
        self.input.write_all(format!("{record}\n").as_bytes()).await.unwrap();
        self.input.flush().await.unwrap();
    }

    /// Send a request; returns its id.
    pub async fn send_request(&mut self, method: &str, params: Value) -> i64 {
        self.next_id += 1;
        let id = self.next_id;
        self.write(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await;
        id
    }

    pub async fn notify(&mut self, method: &str, params: Value) {
        self.write(json!({"jsonrpc": "2.0", "method": method, "params": params})).await;
    }

    /// Read until a record matches; every line must be JSON-RPC 2.0.
    pub async fn until(&mut self, predicate: impl Fn(&Value) -> bool) -> Value {
        let read = async {
            loop {
                if let Some(found) = self.seen[self.cursor..].iter().position(&predicate) {
                    self.cursor += found + 1;
                    return self.seen[self.cursor - 1].clone();
                }
                let line = self.lines.recv().await.expect("agent output open");
                let record: Value = serde_json::from_str(&line).unwrap_or_else(|_| panic!("not JSON: {line}"));
                assert_eq!(record["jsonrpc"], "2.0", "{line}");
                self.seen.push(record);
            }
        };
        tokio::time::timeout(Duration::from_secs(30), read).await.expect("record in time")
    }

    /// The response to request `id`, received earlier or next.
    pub async fn response(&mut self, id: i64) -> Value {
        let is_response = move |record: &Value| record["id"] == id && record.get("method").is_none();
        if let Some(found) = self.seen.iter().find(|record| is_response(record)) {
            return found.clone();
        }
        self.until(is_response).await
    }

    pub async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.send_request(method, params).await;
        self.response(id).await
    }

    /// `session/update` bodies for `session_id` received so far.
    pub fn updates(&self, session_id: &str) -> Vec<Value> {
        self.seen
            .iter()
            .filter(|record| record["method"] == "session/update" && record["params"]["sessionId"] == session_id)
            .map(|record| record["params"]["update"].clone())
            .collect()
    }

    pub fn clear(&mut self) {
        self.seen.clear();
        self.cursor = 0;
    }

    /// Answer a request the agent sent.
    pub async fn respond(&mut self, id: Value, result: Value) {
        self.write(json!({"jsonrpc": "2.0", "id": id, "result": result})).await;
    }
}
