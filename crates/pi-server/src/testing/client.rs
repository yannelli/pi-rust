//! A raw protocol client that records every server message, for wire-level tests.

use std::sync::{Arc, Mutex, OnceLock};

use pi_chord::services::ServiceCall;
use pi_chord::{BoxFuture, ChordError, ChordResult, json};
use pi_protocol::{
    ClientHello, ClientMessage, FrameDecoderOptions, RequestEnvelope, ResponseEnvelope, RpcTarget, ServerMessage,
    ServerMessageDecoder, SessionTarget, encode_client_message,
};
use tokio::sync::Notify;

use crate::connection::{ByteConnection, ByteConnectionHandler};
use crate::server::Server;
use crate::task::lock;
use crate::types::ServerHost;

/// The client side of one test connection.
pub trait WireChannel: Send + Sync {
    fn send(&self, chunk: Vec<u8>) -> BoxFuture<ChordResult<()>>;
    fn send_fragmented(&self, chunk: Vec<u8>, split_at: usize) -> BoxFuture<ChordResult<()>>;
    fn close(&self) -> BoxFuture<()>;
}

#[derive(Default)]
struct State {
    messages: Vec<ServerMessage>,
    decoder: ServerMessageDecoder,
    request_sequence: u64,
    attachment: Option<(String, String)>,
    closed: bool,
    failure: Option<String>,
}

pub struct ProtocolTestClient {
    channel: OnceLock<Arc<dyn WireChannel>>,
    state: Mutex<State>,
    changed: Notify,
}

fn encode(message: &ClientMessage) -> Vec<u8> {
    encode_client_message(message, &FrameDecoderOptions::default()).unwrap_or_else(|error| panic!("{error}"))
}

impl ProtocolTestClient {
    pub fn new(channel: Arc<dyn WireChannel>) -> Arc<Self> {
        let client = Self::unbound();
        let _ = client.channel.set(channel);
        client
    }

    fn unbound() -> Arc<Self> {
        Arc::new(Self { channel: OnceLock::new(), state: Mutex::new(State::default()), changed: Notify::new() })
    }

    fn channel(&self) -> &Arc<dyn WireChannel> {
        self.channel.get().unwrap_or_else(|| panic!("test client has no channel"))
    }

    pub fn messages(&self) -> Vec<ServerMessage> {
        lock(&self.state).messages.clone()
    }

    pub fn closed(&self) -> bool {
        lock(&self.state).closed
    }

    pub async fn hello(&self, version: u64) -> ChordResult<ServerMessage> {
        self.send_message(ClientMessage::Hello(ClientHello { version })).await?;
        self.next(|message| matches!(message, ServerMessage::Hello(_) | ServerMessage::HelloError(_))).await
    }

    pub async fn request_service(
        &self,
        target: RpcTarget,
        call: ServiceCall,
        id: Option<String>,
    ) -> ChordResult<ResponseEnvelope> {
        let id = id.unwrap_or_else(|| {
            let mut state = lock(&self.state);
            state.request_sequence += 1;
            format!("request-{}", state.request_sequence)
        });
        let request = RequestEnvelope { id: id.clone(), target, call: call.to_json() };
        self.send_message(ClientMessage::Request(request)).await?;
        let response = self.next(|message| matches!(message, ServerMessage::Response(r) if r.id == id)).await?;
        match response {
            ServerMessage::Response(response) => Ok(response),
            _ => unreachable!("the predicate selects responses"),
        }
    }

    pub async fn attach(&self, server_id: &str, session_id: &str) -> ChordResult<ResponseEnvelope> {
        let call = ServiceCall {
            service_id: "pi.session-management".into(),
            instance: None,
            member: "attach".into(),
            args: vec![json!(session_id)],
        };
        self.request_service(RpcTarget::server(server_id), call, None).await
    }

    /// Call a Session service through the latest attachment, or a missing one when not attached.
    pub async fn request_session_service(
        &self,
        server_id: &str,
        session_id: &str,
        call: ServiceCall,
        id: Option<String>,
    ) -> ChordResult<ResponseEnvelope> {
        let attachment_id = match &lock(&self.state).attachment {
            Some((session, attachment)) if session == session_id => attachment.clone(),
            _ => "missing-attachment".to_string(),
        };
        let target = SessionTarget { server_id: server_id.into(), session_id: session_id.into(), attachment_id };
        self.request_service(RpcTarget::Session(target), call, id).await
    }

    pub async fn send_message(&self, message: ClientMessage) -> ChordResult<()> {
        self.channel().send(encode(&message)).await
    }

    pub async fn send_bytes(&self, chunk: Vec<u8>) -> ChordResult<()> {
        self.channel().send(chunk).await
    }

    pub async fn send_fragmented_message(&self, message: ClientMessage, split_at: usize) -> ChordResult<()> {
        self.channel().send_fragmented(encode(&message), split_at).await
    }

    /// The first recorded message matching `predicate`, waiting for one to arrive.
    pub async fn next(&self, predicate: impl Fn(&ServerMessage) -> bool) -> ChordResult<ServerMessage> {
        self.next_from(0, predicate).await
    }

    pub async fn next_from(
        &self,
        index: usize,
        predicate: impl Fn(&ServerMessage) -> bool,
    ) -> ChordResult<ServerMessage> {
        loop {
            let changed = self.changed.notified();
            {
                let state = lock(&self.state);
                if let Some(found) = state.messages.iter().skip(index).find(|message| predicate(message)) {
                    return Ok(found.clone());
                }
                if let Some(failure) = &state.failure {
                    return Err(ChordError::error(failure.clone()));
                }
            }
            changed.await;
        }
    }

    pub async fn wait_for_close(&self) {
        loop {
            let changed = self.changed.notified();
            if lock(&self.state).closed {
                return;
            }
            changed.await;
        }
    }

    pub async fn close(&self) {
        self.channel().close().await
    }

    pub fn receive(&self, chunk: &[u8]) {
        let mut state = lock(&self.state);
        match state.decoder.push(chunk) {
            Ok(messages) => {
                for message in messages {
                    if let ServerMessage::Attachment(envelope) = &message {
                        state.attachment = envelope
                            .attachment
                            .as_ref()
                            .map(|target| (target.session_id.clone(), target.attachment_id.clone()));
                    }
                    state.messages.push(message);
                }
            }
            Err(error) => state.failure = Some(error.0),
        }
        drop(state);
        self.changed.notify_waiters();
    }

    pub fn mark_closed(&self) {
        {
            let mut state = lock(&self.state);
            if state.closed {
                return;
            }
            state.closed = true;
            state.failure.get_or_insert_with(|| "Wire connection closed".into());
        }
        self.changed.notify_waiters();
    }
}

struct MemoryConnection {
    client: Arc<ProtocolTestClient>,
    closed: Mutex<bool>,
}

impl ByteConnection for MemoryConnection {
    fn closed(&self) -> bool {
        *lock(&self.closed)
    }

    fn send(&self, chunk: Vec<u8>) -> BoxFuture<ChordResult<()>> {
        self.client.receive(&chunk);
        Box::pin(std::future::ready(Ok(())))
    }

    fn close(&self, final_chunk: Option<Vec<u8>>) -> BoxFuture<ChordResult<()>> {
        if let Some(chunk) = final_chunk {
            self.client.receive(&chunk);
        }
        *lock(&self.closed) = true;
        self.client.mark_closed();
        Box::pin(std::future::ready(Ok(())))
    }
}

struct MemoryChannel {
    handler: OnceLock<ByteConnectionHandler>,
    connection: Arc<MemoryConnection>,
}

impl MemoryChannel {
    fn handler(&self) -> &ByteConnectionHandler {
        self.handler.get().unwrap_or_else(|| panic!("connection was not accepted"))
    }
}

impl WireChannel for MemoryChannel {
    fn send(&self, chunk: Vec<u8>) -> BoxFuture<ChordResult<()>> {
        self.handler().on_data(&chunk);
        Box::pin(std::future::ready(Ok(())))
    }

    fn send_fragmented(&self, chunk: Vec<u8>, split_at: usize) -> BoxFuture<ChordResult<()>> {
        self.handler().on_data(&chunk[..split_at]);
        self.handler().on_data(&chunk[split_at..]);
        Box::pin(std::future::ready(Ok(())))
    }

    fn close(&self) -> BoxFuture<()> {
        let already = std::mem::replace(&mut *lock(&self.connection.closed), true);
        if !already {
            self.handler().on_close();
            self.connection.client.mark_closed();
        }
        Box::pin(std::future::ready(()))
    }
}

/// Connect a test client to `server` through synchronous in-memory byte delivery.
pub fn connect_in_memory<H: ServerHost>(server: &Server<H>) -> Arc<ProtocolTestClient> {
    let client = ProtocolTestClient::unbound();
    let connection = Arc::new(MemoryConnection { client: client.clone(), closed: Mutex::new(false) });
    let channel = Arc::new(MemoryChannel { handler: OnceLock::new(), connection: connection.clone() });
    let _ = client.channel.set(channel.clone());
    let _ = channel.handler.set(server.accept(connection));
    client
}
