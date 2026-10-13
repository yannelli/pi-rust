//! Port of upstream `test/support.ts`: an in-memory byte server that records client messages.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_chord::BoxFuture;
use pi_client::{ByteTransport, ByteTransportFactory, ByteTransportHandlers, ClientError};
use pi_protocol::{
    ClientMessage, ClientMessageDecoder, FrameDecoderOptions, ServerHello, ServerMessage, encode_server_message,
};
use tokio::sync::Notify;

pub const SERVER_ID: &str = "00000000-0000-4000-8000-000000000001";

#[derive(Default)]
struct State {
    messages: Vec<ClientMessage>,
    handlers: Option<(u64, ByteTransportHandlers)>,
    decoder: ClientMessageDecoder,
    client_close_count: usize,
    next_connection: u64,
}

pub struct MemoryByteServer {
    pub server_id: String,
    state: Mutex<State>,
    changed: Notify,
}

struct MemoryTransport {
    server: Arc<MemoryByteServer>,
    id: u64,
    closed: Mutex<bool>,
}

impl MemoryByteServer {
    pub fn new() -> Arc<Self> {
        Self::with_id(SERVER_ID)
    }

    pub fn with_id(server_id: &str) -> Arc<Self> {
        Arc::new(Self { server_id: server_id.into(), state: Mutex::default(), changed: Notify::new() })
    }

    pub fn connect(self: &Arc<Self>, handlers: ByteTransportHandlers) -> Arc<dyn ByteTransport> {
        let mut state = self.state.lock().unwrap();
        state.next_connection += 1;
        let id = state.next_connection;
        state.handlers = Some((id, handlers));
        state.decoder = ClientMessageDecoder::default();
        Arc::new(MemoryTransport { server: self.clone(), id, closed: Mutex::new(false) })
    }

    pub fn factory(self: &Arc<Self>) -> ByteTransportFactory {
        let server = self.clone();
        Arc::new(move |handlers| {
            let transport = server.connect(handlers);
            Box::pin(async move { Ok(transport) })
        })
    }

    pub fn messages(&self) -> Vec<ClientMessage> {
        self.state.lock().unwrap().messages.clone()
    }

    pub fn client_close_count(&self) -> usize {
        self.state.lock().unwrap().client_close_count
    }

    pub async fn wait_for_messages(&self, count: usize) {
        let wait = async {
            loop {
                let changed = self.changed.notified();
                if self.state.lock().unwrap().messages.len() >= count {
                    return;
                }
                changed.await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), wait).await.expect("messages arrive");
    }

    fn handlers(&self) -> ByteTransportHandlers {
        let state = self.state.lock().unwrap();
        state.handlers.as_ref().map(|(_, handlers)| handlers.clone()).expect("No client connection")
    }

    pub fn send(&self, message: ServerMessage) {
        let frame = encode_server_message(&message, &FrameDecoderOptions::default()).unwrap();
        self.handlers().on_data(&frame);
    }

    pub fn send_raw(&self, chunk: &[u8]) {
        self.handlers().on_data(chunk);
    }

    pub fn disconnect(&self) {
        let handlers = self.state.lock().unwrap().handlers.take();
        if let Some((_, handlers)) = handlers {
            handlers.on_close();
        }
    }

    pub fn error(&self, error: ClientError) {
        let handlers = self.state.lock().unwrap().handlers.take();
        if let Some((_, handlers)) = handlers {
            handlers.on_error(error);
        }
    }
}

impl ByteTransport for MemoryTransport {
    fn send(&self, chunk: Vec<u8>) -> BoxFuture<Result<(), ClientError>> {
        let messages = self.server.state.lock().unwrap().decoder.push(&chunk).unwrap();
        for message in messages {
            let hello = matches!(message, ClientMessage::Hello(_));
            self.server.state.lock().unwrap().messages.push(message);
            self.server.changed.notify_waiters();
            if hello {
                self.server.send(ServerMessage::Hello(ServerHello { server_id: self.server.server_id.clone() }));
            }
        }
        Box::pin(std::future::ready(Ok(())))
    }

    fn close(&self) {
        let mut closed = self.closed.lock().unwrap();
        if *closed {
            return;
        }
        *closed = true;
        let mut state = self.server.state.lock().unwrap();
        state.client_close_count += 1;
        if state.handlers.as_ref().is_some_and(|(id, _)| *id == self.id) {
            state.handlers = None;
        }
    }
}

/// Wait until `condition` holds, failing after five seconds.
pub async fn poll(mut condition: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(tokio::time::Instant::now() < deadline, "condition was not met in time");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}
