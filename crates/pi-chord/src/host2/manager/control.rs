//! Control messages, which share the IPC channel with chord frames as `{ chord2control: message }`, and the
//! child's side of them ([`ChildControl`]).

use std::sync::{Arc, Mutex};

use tokio::sync::watch;

use crate::error::{ChordError, ChordResult};
use crate::host2::errors::ChordErrorCode;
use crate::host2::link::{IpcChannel, StreamChannel};
use crate::host2::process_lock::{hold_for_process, process_lock};
use crate::json::{Json, JsonMap};
use crate::sync::{BoxFuture, lock};

/// The lock name a child acquires ([`ChildControl::lock`]). A child gets only the lock its driver gives it.
pub const LOCK_ENV: &str = "CHORD2_LOCK";
/// How a child reaches its parent's channel: `stdin` on Unix, a named pipe's path on Windows.
pub const IPC_ENV: &str = "CHORD2_IPC";

/// Work a child reports; missing counts are zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StatusReport {
    pub active_calls: Option<u64>,
    pub viewers: Option<u64>,
    pub durable_tasks: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ControlMessage {
    Init { value: Json },
    Ready,
    Fatal { message: String, reason: Option<String> },
    Shutdown,
    Status(StatusReport),
}

impl ControlMessage {
    /// The message object, fields in upstream's order.
    pub fn to_json(&self) -> Json {
        let mut map = JsonMap::new();
        let kind = match self {
            Self::Init { .. } => "init",
            Self::Ready => "ready",
            Self::Fatal { .. } => "fatal",
            Self::Shutdown => "shutdown",
            Self::Status(_) => "status",
        };
        map.insert("type", Json::from(kind));
        match self {
            Self::Init { value } => {
                map.insert("value", value.clone());
            }
            Self::Fatal { message, reason } => {
                map.insert("message", Json::from(message.as_str()));
                if let Some(reason) = reason {
                    map.insert("reason", Json::from(reason.as_str()));
                }
            }
            Self::Status(status) => {
                let counts = [
                    ("activeCalls", status.active_calls),
                    ("viewers", status.viewers),
                    ("durableTasks", status.durable_tasks),
                ];
                for (name, count) in counts {
                    if let Some(count) = count {
                        map.insert(name, Json::from(count));
                    }
                }
            }
            Self::Ready | Self::Shutdown => {}
        }
        Json::object(map)
    }

    /// The message, if `value` is one.
    pub fn from_json(value: &Json) -> Option<Self> {
        let text = |name: &str| value.get(name).and_then(Json::as_str).map(String::from);
        let count = |name: &str| value.get(name).and_then(Json::as_u64);
        Some(match value.get("type")?.as_str()? {
            "init" => Self::Init { value: value.get("value").cloned().unwrap_or(Json::Null) },
            "ready" => Self::Ready,
            "fatal" => Self::Fatal { message: text("message")?, reason: text("reason") },
            "shutdown" => Self::Shutdown,
            "status" => Self::Status(StatusReport {
                active_calls: count("activeCalls"),
                viewers: count("viewers"),
                durable_tasks: count("durableTasks"),
            }),
            _ => return None,
        })
    }

    /// The channel message: `{ chord2control: message }`.
    pub fn wrap(&self) -> Json {
        let mut map = JsonMap::new();
        map.insert("chord2control", self.to_json());
        Json::object(map)
    }

    /// The control message a channel message carries, if any.
    pub fn unwrap(message: &Json) -> Option<Self> {
        Self::from_json(message.get("chord2control")?)
    }
}

pub type ShutdownHandler = Arc<dyn Fn() -> BoxFuture<()> + Send + Sync>;

/// The child side of the control channel (upstream `childControl()`).
pub struct ChildControl {
    channel: Arc<dyn IpcChannel>,
    init: watch::Receiver<Option<Json>>,
    shutdown: Arc<Mutex<Vec<ShutdownHandler>>>,
}

impl ChildControl {
    /// Control on `channel`; the child's host links to the same channel ([`crate::host2::link::ipc_link`]).
    pub fn new(channel: Arc<dyn IpcChannel>) -> Self {
        let (init_tx, init) = watch::channel(None);
        let shutdown: Arc<Mutex<Vec<ShutdownHandler>>> = Arc::default();
        let handlers = shutdown.clone();
        channel.on_message(Arc::new(move |message| match ControlMessage::unwrap(message) {
            Some(ControlMessage::Init { value }) => {
                init_tx.send_if_modified(|init| init.is_none() && init.replace(value).is_none());
            }
            Some(ControlMessage::Shutdown) => {
                for handler in lock(&handlers).clone() {
                    tokio::spawn(handler());
                }
            }
            _ => {}
        }));
        Self { channel, init, shutdown }
    }

    /// Control on the channel of the process that spawned this one with [`super::ForkDriver`]. Must be called
    /// within a Tokio runtime.
    pub fn from_env() -> ChordResult<Self> {
        let Ok(address) = std::env::var(IPC_ENV) else {
            let message = format!("no parent channel: {IPC_ENV} is not set (spawn this process with ForkDriver)");
            return Err(ChordError::code_only(ChordErrorCode::Unavailable, message));
        };
        let channel: Arc<StreamChannel> = super::os::child_channel(&address).map_err(ChordError::other)?;
        Ok(Self::new(channel))
    }

    pub fn channel(&self) -> Arc<dyn IpcChannel> {
        self.channel.clone()
    }

    /// The parent's `init` value; fails when the channel closes first.
    pub async fn init(&self) -> ChordResult<Json> {
        let mut init = self.init.clone();
        let value = init.wait_for(Option::is_some).await.map(|value| value.clone().unwrap_or(Json::Null));
        value.map_err(|_| ChordError::code_only(ChordErrorCode::Unavailable, "parent channel closed before init"))
    }

    /// Acquires the manager's lock (if one was configured) as a kernel lock held for this process's lifetime. Call
    /// it before opening storage. Fails with `unavailable/locked`, also reported to the manager as a fatal error.
    pub async fn lock(&self) -> ChordResult<()> {
        let Ok(name) = std::env::var(LOCK_ENV) else { return Ok(()) };
        match process_lock(&name).await {
            Ok(held) => {
                hold_for_process(held);
                Ok(())
            }
            Err(error) => {
                self.fatal(&error);
                Err(error)
            }
        }
    }

    pub fn ready(&self) {
        self.send(ControlMessage::Ready);
    }

    pub fn fatal(&self, error: &ChordError) {
        let reason = error.reason().map(String::from);
        self.send(ControlMessage::Fatal { message: error.to_string(), reason });
    }

    pub fn report(&self, status: StatusReport) {
        self.send(ControlMessage::Status(status));
    }

    /// Runs on every `shutdown` from the parent.
    pub fn on_shutdown(&self, handler: ShutdownHandler) {
        lock(&self.shutdown).push(handler);
    }

    /// Resolves once the messages sent so far are written: call it before the process exits.
    pub async fn flush(&self) {
        self.channel.flushed().await;
    }

    /// A parent that is gone fails the write; the disconnect that follows is how a child notices.
    fn send(&self, message: ControlMessage) {
        if self.channel.connected() {
            self.channel.send(message.wrap(), Some(Box::new(|_| {})));
        }
    }
}
