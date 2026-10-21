//! Node IPC links (upstream `link.ts`): chord frames travel on a process message channel as `{ chord2: frame }`,
//! and other messages (control) pass by. [`StreamChannel`] is such a channel over a byte stream, in the bytes of
//! Node's JSON serialization: `JSON.stringify(message)` and a newline per message.

use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::AbortHandle;

use super::pack::{pack_object, unpack_object};
use super::{BaseLink, Link, Transport};
use crate::host2::wire::Frame;
use crate::json::{Json, JsonMap};
use crate::sync::{BoxFuture, lock};

pub type ListenerId = u64;
pub type MessageListener = Arc<dyn Fn(&Json) + Send + Sync>;
pub type DisconnectListener = Arc<dyn Fn() + Send + Sync>;
/// Gets the outcome of one write: a failed one (the peer process is gone) is reported here only.
pub type SendCallback = Box<dyn FnOnce(Result<(), IpcSendError>) + Send>;

/// Why a message was not written, with Node's error code when there is one (`EPIPE`).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct IpcSendError {
    pub code: Option<String>,
    pub message: String,
}

impl IpcSendError {
    pub fn new(code: Option<&str>, message: impl Into<String>) -> Self {
        Self { code: code.map(String::from), message: message.into() }
    }

    /// The code, or the message without one (upstream's `error.code ?? error.message`).
    pub fn label(&self) -> &str {
        self.code.as_deref().unwrap_or(&self.message)
    }
}

impl From<&io::Error> for IpcSendError {
    fn from(error: &io::Error) -> Self {
        let code = match error.kind() {
            io::ErrorKind::BrokenPipe => Some("EPIPE"),
            io::ErrorKind::ConnectionReset => Some("ECONNRESET"),
            io::ErrorKind::ConnectionAborted => Some("ECONNABORTED"),
            io::ErrorKind::NotConnected => Some("ENOTCONN"),
            _ => None,
        };
        Self::new(code, error.to_string())
    }
}

/// A process message channel: Node's `process` in a forked child, or the parent's `ChildProcess`.
pub trait IpcChannel: Send + Sync {
    fn connected(&self) -> bool;
    /// Queues one message; `false` when the channel is closed. A failed write goes to `done` only.
    fn send(&self, message: Json, done: Option<SendCallback>) -> bool;
    fn on_message(&self, listener: MessageListener) -> ListenerId;
    fn on_disconnect(&self, listener: DisconnectListener) -> ListenerId;
    fn off(&self, id: ListenerId);
    /// Resolves once every message queued before the call was written (or the channel closed).
    fn flushed(&self) -> BoxFuture<()> {
        Box::pin(async {})
    }
}

/// The IPC side of [`BaseLink`]. Large payloads are packed both ways; the messages are serialized by the channel,
/// so the link reports no sizes and is not windowed (both ends are local processes).
pub struct IpcTransport {
    channel: Arc<dyn IpcChannel>,
    link: Weak<IpcLink>,
    listeners: Mutex<Vec<ListenerId>>,
}

pub type IpcLink = BaseLink<IpcTransport>;

impl Transport for IpcTransport {
    fn transmit(&self, frame: &Frame) -> Option<usize> {
        if !self.channel.connected() {
            return None;
        }
        let mut message = JsonMap::new();
        message.insert("chord2", pack_object(frame, true));
        // A write can fail after the peer exited, before the channel reports it: the link ends either way.
        let link = self.link.clone();
        let done: SendCallback = Box::new(move |result| {
            if let (Err(error), Some(link)) = (result, link.upgrade()) {
                link.ended(&format!("ipc send failed: {}", error.label()));
            }
        });
        self.channel.send(Json::object(message), Some(done));
        None
    }

    fn shutdown(&self) {
        let listeners = std::mem::take(&mut *lock(&self.listeners));
        for id in listeners {
            self.channel.off(id);
        }
    }
}

/// A link on an IPC channel: `{ chord2: frame }` messages are frames, everything else is left to other listeners.
pub fn ipc_link(channel: Arc<dyn IpcChannel>) -> Arc<IpcLink> {
    let link = Arc::new_cyclic(|link| {
        BaseLink::new(IpcTransport { channel: channel.clone(), link: link.clone(), listeners: Mutex::default() })
    });
    let weak = Arc::downgrade(&link);
    let on_message = channel.on_message(Arc::new(move |message: &Json| {
        let (Some(link), Some(frame)) = (weak.upgrade(), message.get("chord2")) else { return };
        match unpack_object(frame) {
            Ok(received) => link.deliver(received),
            Err(_) => link.close(Some("protocol error")),
        }
    }));
    let weak = Arc::downgrade(&link);
    let on_disconnect = channel.on_disconnect(Arc::new(move || {
        if let Some(link) = weak.upgrade() {
            link.ended("ipc disconnected");
        }
    }));
    lock(&link.transport().listeners).extend([on_message, on_disconnect]);
    link
}

enum Outgoing {
    Line(Vec<u8>, Option<SendCallback>),
    Flush(oneshot::Sender<()>),
}

#[derive(Default)]
struct Listeners {
    next: ListenerId,
    message: Vec<(ListenerId, MessageListener)>,
    disconnect: Vec<(ListenerId, DisconnectListener)>,
}

struct ChannelState {
    connected: AtomicBool,
    outgoing: mpsc::UnboundedSender<Outgoing>,
    listeners: Mutex<Listeners>,
    /// Set by the first message listener; reading waits for it, so no early message reaches nobody.
    listening: watch::Sender<bool>,
    tasks: Mutex<Vec<AbortHandle>>,
}

impl ChannelState {
    fn dispatch(&self, message: &Json) {
        let listeners: Vec<_> = lock(&self.listeners).message.iter().map(|(_, listener)| listener.clone()).collect();
        for listener in listeners {
            listener(message);
        }
    }

    /// Disconnects once: stops the transfer and tells the disconnect listeners.
    fn end(&self) {
        if !self.connected.swap(false, Ordering::SeqCst) {
            return;
        }
        let listeners = {
            let mut listeners = lock(&self.listeners);
            listeners.message.clear();
            std::mem::take(&mut listeners.disconnect)
        };
        for task in std::mem::take(&mut *lock(&self.tasks)) {
            task.abort();
        }
        for (_, listener) in listeners {
            listener();
        }
    }
}

/// [`IpcChannel`] over a byte stream. Messages sent before the stream connects wait, in order. Reading starts with
/// the first message listener; the end of the stream (or a line that is not JSON) disconnects the channel.
pub struct StreamChannel {
    state: Arc<ChannelState>,
}

impl StreamChannel {
    /// Runs the channel on the stream `connect` resolves to. Must be called within a Tokio runtime.
    pub fn new<S, F>(connect: F) -> Arc<Self>
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
        F: Future<Output = io::Result<S>> + Send + 'static,
    {
        let (outgoing, queue) = mpsc::unbounded_channel();
        let state = Arc::new(ChannelState {
            connected: AtomicBool::new(true),
            outgoing,
            listeners: Mutex::default(),
            listening: watch::Sender::new(false),
            tasks: Mutex::default(),
        });
        let task = tokio::spawn(run(connect, queue, state.clone()));
        lock(&state.tasks).push(task.abort_handle());
        Arc::new(Self { state })
    }

    /// Closes the channel from this side.
    pub fn disconnect(&self) {
        self.state.end();
    }
}

impl IpcChannel for StreamChannel {
    fn connected(&self) -> bool {
        self.state.connected.load(Ordering::SeqCst)
    }

    fn send(&self, message: Json, done: Option<SendCallback>) -> bool {
        if !self.connected() {
            if let Some(done) = done {
                done(Err(IpcSendError::new(Some("ERR_IPC_CHANNEL_CLOSED"), "Channel closed")));
            }
            return false;
        }
        let mut line = message.to_string().into_bytes();
        line.push(b'\n');
        self.state.outgoing.send(Outgoing::Line(line, done)).is_ok()
    }

    fn on_message(&self, listener: MessageListener) -> ListenerId {
        let mut listeners = lock(&self.state.listeners);
        listeners.next += 1;
        let id = listeners.next;
        listeners.message.push((id, listener));
        self.state.listening.send_replace(true);
        id
    }

    fn on_disconnect(&self, listener: DisconnectListener) -> ListenerId {
        let mut listeners = lock(&self.state.listeners);
        listeners.next += 1;
        let id = listeners.next;
        listeners.disconnect.push((id, listener));
        id
    }

    fn off(&self, id: ListenerId) {
        let mut listeners = lock(&self.state.listeners);
        listeners.message.retain(|(entry, _)| *entry != id);
        listeners.disconnect.retain(|(entry, _)| *entry != id);
    }

    fn flushed(&self) -> BoxFuture<()> {
        let (done, flushed) = oneshot::channel();
        let queued = self.state.outgoing.send(Outgoing::Flush(done)).is_ok();
        Box::pin(async move {
            if queued {
                let _ = flushed.await;
            }
        })
    }
}

async fn run<S, F>(connect: F, queue: mpsc::UnboundedReceiver<Outgoing>, state: Arc<ChannelState>)
where
    S: AsyncRead + AsyncWrite + Send + 'static,
    F: Future<Output = io::Result<S>> + Send + 'static,
{
    let Ok(stream) = connect.await else { return state.end() };
    let (read, write) = tokio::io::split(stream);
    let writer = tokio::spawn(write_lines(write, queue));
    lock(&state.tasks).push(writer.abort_handle());
    let _ = state.listening.subscribe().wait_for(|listening| *listening).await;
    read_lines(read, &state).await;
    state.end();
}

async fn write_lines<S: AsyncWrite>(mut write: WriteHalf<S>, mut queue: mpsc::UnboundedReceiver<Outgoing>) {
    while let Some(item) = queue.recv().await {
        match item {
            Outgoing::Line(line, done) => {
                let mut result = write.write_all(&line).await;
                if result.is_ok() {
                    result = write.flush().await;
                }
                if let Some(done) = done {
                    done(result.map_err(|error| IpcSendError::from(&error)));
                }
            }
            Outgoing::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
}

/// Reads one message per line until the stream ends; an unterminated last line is dropped, as Node does.
async fn read_lines<S: AsyncRead>(read: ReadHalf<S>, state: &ChannelState) {
    let mut reader = BufReader::new(read);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line).await {
            Ok(_) if line.pop() == Some(b'\n') => {}
            _ => return,
        }
        let Some(message) = std::str::from_utf8(&line).ok().and_then(|text| Json::parse(text).ok()) else { return };
        state.dispatch(&message);
    }
}
