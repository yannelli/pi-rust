//! Two linked transports in one process, for tests and embedded servers.

use std::sync::{Arc, Mutex, OnceLock, Weak};

use async_trait::async_trait;
use tokio::sync::mpsc;

use super::{McpTransport, TransportEvent, TransportSink};
use crate::error::McpError;
use crate::jsonrpc::JsonRpcMessage;

#[derive(Default)]
struct State {
    started: bool,
    closed: bool,
    sink: Option<TransportSink>,
}

#[derive(Default)]
pub struct InMemoryTransport {
    state: Mutex<State>,
    peer: OnceLock<Weak<InMemoryTransport>>,
}

/// A client and a server transport, each delivering what it sends to the other.
pub fn in_memory_pair() -> (Arc<InMemoryTransport>, Arc<InMemoryTransport>) {
    let client = Arc::new(InMemoryTransport::default());
    let server = Arc::new(InMemoryTransport::default());
    let _ = client.peer.set(Arc::downgrade(&server));
    let _ = server.peer.set(Arc::downgrade(&client));
    (client, server)
}

impl InMemoryTransport {
    /// Start with a new channel and return its receiving end, for the side a test drives.
    pub async fn start_channel(&self) -> Result<mpsc::UnboundedReceiver<TransportEvent>, McpError> {
        let (sink, receiver) = TransportSink::channel();
        self.start(sink).await?;
        Ok(receiver)
    }

    /// Report a transport-level failure, as a real transport would.
    pub fn emit_error(&self, error: McpError) {
        if let Some(sink) = self.lock().sink.clone() {
            sink.error(error);
        }
    }

    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn peer(&self) -> Option<Arc<InMemoryTransport>> {
        self.peer.get().and_then(Weak::upgrade)
    }

    fn deliver(&self, message: JsonRpcMessage) -> bool {
        let state = self.lock();
        match (&state.sink, state.started && !state.closed) {
            (Some(sink), true) => {
                sink.message(message);
                true
            }
            _ => false,
        }
    }

    fn close_now(&self) {
        let sink = {
            let mut state = self.lock();
            if state.closed {
                return;
            }
            state.closed = true;
            state.sink.clone()
        };
        if let Some(sink) = sink {
            sink.close();
        }
        if let Some(peer) = self.peer() {
            peer.close_now();
        }
    }
}

#[async_trait]
impl McpTransport for InMemoryTransport {
    async fn start(&self, sink: TransportSink) -> Result<(), McpError> {
        let mut state = self.lock();
        if state.closed {
            return Err(McpError::closed());
        }
        state.started = true;
        state.sink = Some(sink);
        Ok(())
    }

    async fn send(&self, message: &JsonRpcMessage) -> Result<(), McpError> {
        {
            let state = self.lock();
            if !state.started || state.closed {
                return Err(McpError::closed());
            }
        }
        let delivered = self.peer().is_some_and(|peer| peer.deliver(message.clone()));
        if delivered { Ok(()) } else { Err(McpError::ConnectionClosed("In-memory MCP peer is not connected".into())) }
    }

    async fn close(&self) {
        self.close_now();
    }
}
