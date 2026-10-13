//! Transports carry JSON-RPC messages between the client and a server.

pub mod http;
pub mod memory;
pub mod process;
pub mod sse;
pub mod stdio;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::error::McpError;
use crate::jsonrpc::JsonRpcMessage;

/// Largest message a transport accepts.
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug)]
pub enum TransportEvent {
    Message(JsonRpcMessage),
    /// Reported only: pending requests fail when the transport closes.
    Error(McpError),
    Closed,
}

/// Where a transport delivers what it receives. `close` is delivered at most once.
#[derive(Clone)]
pub struct TransportSink {
    sender: mpsc::UnboundedSender<TransportEvent>,
    closed: Arc<AtomicBool>,
}

impl TransportSink {
    pub fn channel() -> (Self, mpsc::UnboundedReceiver<TransportEvent>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (Self { sender, closed: Arc::new(AtomicBool::new(false)) }, receiver)
    }

    pub fn message(&self, message: JsonRpcMessage) {
        let _ = self.sender.send(TransportEvent::Message(message));
    }

    pub fn error(&self, error: McpError) {
        let _ = self.sender.send(TransportEvent::Error(error));
    }

    pub fn close(&self) {
        if !self.closed.swap(true, Ordering::SeqCst) {
            let _ = self.sender.send(TransportEvent::Closed);
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

#[async_trait]
pub trait McpTransport: Send + Sync + 'static {
    /// Open the transport. Received messages, errors, and the close go to `sink`.
    async fn start(&self, sink: TransportSink) -> Result<(), McpError>;
    async fn send(&self, message: &JsonRpcMessage) -> Result<(), McpError>;
    async fn close(&self);
    /// Called once `initialize` picked the protocol version.
    fn set_protocol_version(&self, _version: &str) {}
    /// Recent stderr output of a stdio server.
    fn stderr(&self) -> Option<String> {
        None
    }
}

pub type SharedTransport = Arc<dyn McpTransport>;
