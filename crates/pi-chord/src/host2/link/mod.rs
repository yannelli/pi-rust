//! Port of upstream `link.ts`: the [`Link`] trait, the shared plumbing of every transport ([`BaseLink`]), payload
//! packing, the in-memory link pair, IPC links and WebSocket links.

mod ipc;
mod memory;
mod pack;
mod websocket;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

pub use ipc::{
    DisconnectListener, IpcChannel, IpcLink, IpcSendError, IpcTransport, ListenerId, MessageListener, SendCallback,
    StreamChannel, ipc_link,
};
pub use memory::{MemoryLink, MemoryTransport, memory_link_pair};
pub use pack::{
    BadMessage, Message, PACK_THRESHOLD, Packed, inflate_message, pack_message, pack_object, parse_frame,
    split_message, unpack, unpack_message, unpack_object,
};
pub use websocket::{WebSocketLink, WebSocketLinkOptions, WebSocketTransport, connect_web_socket, web_socket_link};

use super::wire::Frame;
use crate::json::Json;

/// What a link hands its receiver: a valid frame, or a message with a string `t` that fails validation (the
/// connection treats it as a protocol violation).
#[derive(Clone, Debug, PartialEq)]
pub enum Received {
    Frame(Frame),
    Malformed(Json),
}

impl Received {
    /// The close reason when this is a `bye`, which the link consumes itself.
    fn bye_reason(&self) -> Option<String> {
        match self {
            Self::Frame(Frame::Bye { reason }) => Some(reason.clone().unwrap_or_else(|| "peer closed".into())),
            Self::Malformed(value) if value.get("t").and_then(Json::as_str) == Some("bye") => {
                Some(match value.get("reason") {
                    None | Some(Json::Null) => "peer closed".into(),
                    Some(Json::String(reason)) => reason.to_string(),
                    Some(other) => other.to_string(),
                })
            }
            _ => None,
        }
    }
}

pub type FrameHandler = Arc<dyn Fn(Received) + Send + Sync>;
pub type CloseHandler = Box<dyn FnOnce(&str) + Send>;

/// An ordered (FIFO), bidirectional frame channel between two hosts.
pub trait Link: Send + Sync {
    fn closed(&self) -> bool;
    /// The size of the encoded frame as sent (UTF-16 length of the JSON text, or bytes of a packed message) when
    /// the transport knows it. Flow control counts these sizes; a link that never reports one is not windowed.
    fn send(&self, frame: &Frame) -> Option<usize>;
    /// Sets the single receiver. Frames arriving earlier are queued.
    fn receive(&self, handler: FrameHandler);
    /// Called once with the close reason; at once with `closed` when the link is already closed.
    fn on_close(&self, handler: CloseHandler);
    /// Closes both ends: the peer gets a `bye` (default reason `closed`).
    fn close(&self, reason: Option<&str>);
}

/// What a transport implements for [`BaseLink`].
pub trait Transport: Send + Sync + 'static {
    fn transmit(&self, frame: &Frame) -> Option<usize>;
    fn shutdown(&self);
}

#[derive(Default)]
struct State {
    handler: Option<FrameHandler>,
    queue: VecDeque<Received>,
    close_handlers: Vec<CloseHandler>,
    closed: bool,
    /// One caller at a time runs the receiver, so frames reach it in order.
    draining: bool,
}

/// Shared plumbing: receive queue, close handlers, `bye`. No lock is held while handlers run.
pub struct BaseLink<T> {
    transport: T,
    state: Mutex<State>,
}

/// Clears `draining` if the receiver panics, so a later frame can drain again.
struct DrainGuard<'a> {
    state: &'a Mutex<State>,
    armed: bool,
}

impl Drop for DrainGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.state.lock().unwrap_or_else(PoisonError::into_inner).draining = false;
        }
    }
}

impl<T: Transport> BaseLink<T> {
    pub fn new(transport: T) -> Self {
        Self { transport, state: Mutex::default() }
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Delivers a message from the transport, after every message delivered before it.
    pub fn deliver(&self, received: Received) {
        let mut state = self.lock();
        if state.closed {
            return;
        }
        if state.handler.is_none() {
            if let Some(reason) = received.bye_reason() {
                drop(state);
                self.ended(&reason);
                return;
            }
            state.queue.push_back(received);
            return;
        }
        state.queue.push_back(received);
        if state.draining {
            return;
        }
        state.draining = true;
        drop(state);
        self.drain();
    }

    fn drain(&self) {
        let mut guard = DrainGuard { state: &self.state, armed: true };
        loop {
            let (handler, received) = {
                let mut state = self.lock();
                let next = if state.closed { None } else { state.queue.pop_front() };
                match (state.handler.clone(), next) {
                    (Some(handler), Some(received)) => (handler, received),
                    _ => {
                        state.draining = false;
                        guard.armed = false;
                        return;
                    }
                }
            };
            match received.bye_reason() {
                Some(reason) => self.ended(&reason),
                None => handler(received),
            }
        }
    }

    /// Marks the link closed (transport ended) and notifies the close handlers once.
    pub fn ended(&self, reason: &str) {
        let handlers = {
            let mut state = self.lock();
            if state.closed {
                return;
            }
            state.closed = true;
            state.queue.clear();
            std::mem::take(&mut state.close_handlers)
        };
        self.transport.shutdown();
        for handler in handlers {
            handler(reason);
        }
    }
}

impl<T: Transport> Link for BaseLink<T> {
    fn closed(&self) -> bool {
        self.lock().closed
    }

    fn send(&self, frame: &Frame) -> Option<usize> {
        if self.closed() {
            return None;
        }
        self.transport.transmit(frame)
    }

    fn receive(&self, handler: FrameHandler) {
        let mut state = self.lock();
        state.handler = Some(handler);
        if state.queue.is_empty() || state.draining || state.closed {
            return;
        }
        state.draining = true;
        drop(state);
        self.drain();
    }

    fn on_close(&self, handler: CloseHandler) {
        let mut state = self.lock();
        if state.closed {
            drop(state);
            handler("closed");
            return;
        }
        state.close_handlers.push(handler);
    }

    fn close(&self, reason: Option<&str>) {
        if self.closed() {
            return;
        }
        let reason = reason.unwrap_or("closed");
        self.transport.transmit(&Frame::Bye { reason: Some(reason.into()) });
        self.ended(reason);
    }
}
