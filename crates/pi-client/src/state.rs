//! Shared client state. Changes happen under one lock and produce effects that run after it
//! is released, so listeners and transports may call back into the client.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use pi_chord::services::{
    ServiceProviderUpdate, ServiceStateDecoder, ServiceSubscriptionSnapshot, ServiceUpdateListener,
};
use pi_chord::{AbortSignal, ChordError, Context, Json};
use pi_protocol::{ServerHello, ServerMessageDecoder, SessionTarget};
use tokio::sync::oneshot;

use crate::errors::ClientError;
use crate::transport::{ByteTransport, ByteTransportFactory};
use crate::types::{
    AttachmentChangeListener, ConnectionState, ConnectionStateChange, ConnectionStateListener, ListenerErrorHandler,
};

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

pub(crate) type HandshakeSender = oneshot::Sender<Result<ServerHello, ClientError>>;
pub(crate) type ReplySender = oneshot::Sender<Result<Reply, ClientError>>;

pub(crate) enum Lifecycle {
    Disconnected,
    Connecting {
        id: u64,
        decoder: ServerMessageDecoder,
        handshake: Option<HandshakeSender>,
        transport: Option<Arc<dyn ByteTransport>>,
    },
    Connected {
        id: u64,
        decoder: ServerMessageDecoder,
        transport: Arc<dyn ByteTransport>,
    },
}

impl Lifecycle {
    pub fn state(&self) -> ConnectionState {
        match self {
            Self::Disconnected => ConnectionState::Disconnected,
            Self::Connecting { .. } => ConnectionState::Connecting,
            Self::Connected { .. } => ConnectionState::Connected,
        }
    }

    pub fn id(&self) -> Option<u64> {
        match self {
            Self::Disconnected => None,
            Self::Connecting { id, .. } | Self::Connected { id, .. } => Some(*id),
        }
    }

    pub fn transport(&self) -> Option<Arc<dyn ByteTransport>> {
        match self {
            Self::Disconnected => None,
            Self::Connecting { transport, .. } => transport.clone(),
            Self::Connected { transport, .. } => Some(transport.clone()),
        }
    }
}

pub(crate) enum Reply {
    Value(Option<Json>),
    Snapshot(ServiceSubscriptionSnapshot),
}

pub(crate) struct Pending {
    pub sender: Option<ReplySender>,
    pub subscription: Option<Arc<ActiveService>>,
    pub abort: Option<(AbortSignal, u64)>,
}

#[derive(Default)]
pub(crate) struct ActiveInner {
    pub decoder: Option<ServiceStateDecoder>,
    pub queued_wire: Vec<Json>,
    pub queued: Vec<ServiceProviderUpdate>,
    pub hydrated: bool,
    pub ready: bool,
    pub delivery: VecDeque<ServiceProviderUpdate>,
    pub delivering: bool,
}

/// One live service subscription and its ordered update delivery.
pub(crate) struct ActiveService {
    pub listener: ServiceUpdateListener,
    pub inner: Mutex<ActiveInner>,
}

pub(crate) enum Effect {
    CloseTransport(Arc<dyn ByteTransport>),
    Handshake(HandshakeSender, Result<ServerHello, ClientError>),
    Reply(ReplySender, Result<Reply, ClientError>),
    StateChange(ConnectionStateChange),
    AttachmentChange(Option<SessionTarget>),
    Deliver(Arc<ActiveService>),
    RemoveAbortListener(AbortSignal, u64),
}

#[derive(Default)]
pub(crate) struct Listeners {
    pub next: u64,
    pub state: Vec<(u64, ConnectionStateListener)>,
    pub attachment: Vec<(u64, AttachmentChangeListener)>,
}

pub(crate) struct State {
    pub lifecycle: Lifecycle,
    pub sequence: u64,
    pub pending: HashMap<String, Pending>,
    pub request_sequence: u64,
    pub subscription_sequence: u64,
    pub hello: Option<ServerHello>,
    pub attachment: Option<SessionTarget>,
    pub disposed: bool,
    pub listeners: Listeners,
    pub services: HashMap<String, Arc<ActiveService>>,
}

pub(crate) struct Inner {
    pub server_id: String,
    pub max_frame_length: u32,
    pub factory: ByteTransportFactory,
    pub on_listener_error: Option<ListenerErrorHandler>,
    pub state: Mutex<State>,
    pub me: Weak<Inner>,
}

impl State {
    pub fn new() -> Self {
        Self {
            lifecycle: Lifecycle::Disconnected,
            sequence: 0,
            pending: HashMap::new(),
            request_sequence: 0,
            subscription_sequence: 0,
            hello: None,
            attachment: None,
            disposed: false,
            listeners: Listeners::default(),
            services: HashMap::new(),
        }
    }

    pub fn set_attachment(&mut self, attachment: Option<SessionTarget>, effects: &mut Vec<Effect>) {
        if self.attachment == attachment {
            return;
        }
        self.attachment = attachment.clone();
        effects.push(Effect::AttachmentChange(attachment));
    }

    pub fn reject_pending(&mut self, error: &ClientError, effects: &mut Vec<Effect>) {
        for (_, pending) in self.pending.drain() {
            if let Some((signal, id)) = pending.abort {
                effects.push(Effect::RemoveAbortListener(signal, id));
            }
            if let Some(sender) = pending.sender {
                effects.push(Effect::Reply(sender, Err(error.clone())));
            }
        }
    }

    /// Enter the disconnected state, rejecting the handshake and every pending request.
    pub fn fail(&mut self, error: ClientError, effects: &mut Vec<Effect>) {
        let lifecycle = std::mem::replace(&mut self.lifecycle, Lifecycle::Disconnected);
        match lifecycle {
            Lifecycle::Disconnected => return,
            Lifecycle::Connecting { handshake: Some(handshake), .. } => {
                effects.push(Effect::Handshake(handshake, Err(error.clone())));
            }
            _ => {}
        }
        self.hello = None;
        self.set_attachment(None, effects);
        self.reject_pending(&error, effects);
        self.services.clear();
        effects.push(Effect::StateChange(ConnectionStateChange {
            state: ConnectionState::Disconnected,
            error: Some(error),
        }));
    }

    pub fn fail_and_close(&mut self, error: ClientError, effects: &mut Vec<Effect>) {
        let transport = self.lifecycle.transport();
        self.fail(error, effects);
        if let Some(transport) = transport {
            effects.push(Effect::CloseTransport(transport));
        }
    }

    pub fn is_current(&self, id: u64) -> bool {
        self.lifecycle.id() == Some(id)
    }
}

impl Inner {
    pub fn me(&self) -> Arc<Inner> {
        self.me.upgrade().unwrap_or_else(|| unreachable!("client methods run on a live Arc"))
    }

    pub fn report_listener_error(&self, error: &ChordError) {
        if let Some(handler) = &self.on_listener_error {
            handler(error);
        }
    }

    /// Lock, mutate, then run the resulting effects without the lock.
    pub fn update<T>(&self, change: impl FnOnce(&mut State, &mut Vec<Effect>) -> T) -> T {
        let mut effects = Vec::new();
        let result = change(&mut lock(&self.state), &mut effects);
        self.run(effects);
        result
    }

    pub fn run(&self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::CloseTransport(transport) => transport.close(),
                Effect::Handshake(sender, result) => {
                    let _ = sender.send(result);
                }
                Effect::Reply(sender, result) => {
                    let _ = sender.send(result);
                }
                Effect::StateChange(change) => {
                    let listeners: Vec<_> = lock(&self.state).listeners.state.iter().map(|(_, l)| l.clone()).collect();
                    for listener in listeners {
                        listener(&change);
                    }
                }
                Effect::AttachmentChange(attachment) => {
                    let listeners: Vec<_> =
                        lock(&self.state).listeners.attachment.iter().map(|(_, l)| l.clone()).collect();
                    for listener in listeners {
                        listener(attachment.as_ref());
                    }
                }
                Effect::Deliver(active) => self.deliver(&active),
                Effect::RemoveAbortListener(signal, id) => signal.remove_listener(id),
            }
        }
    }

    /// Deliver queued updates in order; a concurrent caller leaves the work to the active one.
    pub fn deliver(&self, active: &ActiveService) {
        {
            let mut inner = lock(&active.inner);
            if inner.delivering {
                return;
            }
            inner.delivering = true;
        }
        loop {
            let update = {
                let mut inner = lock(&active.inner);
                match inner.delivery.pop_front() {
                    Some(update) => update,
                    None => {
                        inner.delivering = false;
                        return;
                    }
                }
            };
            if let Err(error) = (active.listener)(update, &Context::background()) {
                self.report_listener_error(&error);
            }
        }
    }
}
