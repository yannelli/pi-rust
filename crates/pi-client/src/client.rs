//! The public client: connection control, routed requests, and listeners.

use std::sync::{Arc, Mutex};

use pi_chord::services::{
    ServiceCall, ServiceCatalogueEntry, create_service_catalogue_call, parse_service_call, parse_service_catalogue,
};
use pi_chord::{AbortSignal, Json};
use pi_protocol::{
    CancelEnvelope, ClientMessage, DEFAULT_MAX_FRAME_LENGTH, RequestEnvelope, RpcTarget, ServerHello, SessionTarget,
    encode_client_message, is_server_id,
};
use tokio::sync::oneshot;

use crate::errors::ClientError;
use crate::state::{ActiveService, Effect, Inner, Lifecycle, Pending, Reply, State, lock};
use crate::types::{AttachmentChangeListener, ClientOptions, ConnectionState, ConnectionStateListener};

/// A client for one logical server over a fresh transport per connection.
#[derive(Clone)]
pub struct Client {
    pub(crate) inner: Arc<Inner>,
}

/// Removes a listener when dropped or when [`ListenerGuard::remove`] runs.
pub struct ListenerGuard {
    remove: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl ListenerGuard {
    pub fn remove(&self) {
        if let Some(remove) = lock(&self.remove).take() {
            remove();
        }
    }

    /// Keep the listener for the client's lifetime.
    pub fn forget(self) {
        lock(&self.remove).take();
    }
}

impl Drop for ListenerGuard {
    fn drop(&mut self) {
        self.remove();
    }
}

impl Client {
    pub fn new(options: ClientOptions) -> Result<Self, ClientError> {
        if !is_server_id(&options.server_id) {
            return Err(ClientError::Type("serverId must be a canonical lowercase UUIDv4".into()));
        }
        let max_frame_length = options.max_frame_length.unwrap_or(DEFAULT_MAX_FRAME_LENGTH);
        if max_frame_length == 0 {
            let message = format!("Client maxFrameLength must be an integer between 1 and {}", u32::MAX);
            return Err(ClientError::Type(message));
        }
        let inner = Arc::new_cyclic(|me| Inner {
            server_id: options.server_id,
            max_frame_length,
            factory: options.transport_factory,
            on_listener_error: options.on_listener_error,
            state: Mutex::new(State::new()),
            me: me.clone(),
        });
        Ok(Self { inner })
    }

    /// Create a client and connect it, disposing it when the connection fails.
    pub async fn connect_new(options: ClientOptions) -> Result<Self, ClientError> {
        let client = Self::new(options)?;
        match client.connect().await {
            Ok(_) => Ok(client),
            Err(error) => {
                client.dispose();
                Err(error)
            }
        }
    }

    pub fn disposed(&self) -> bool {
        lock(&self.inner.state).disposed
    }

    pub fn connection_state(&self) -> ConnectionState {
        lock(&self.inner.state).lifecycle.state()
    }

    pub fn connected(&self) -> bool {
        self.connection_state() == ConnectionState::Connected
    }

    pub fn server_id(&self) -> &str {
        &self.inner.server_id
    }

    pub fn hello(&self) -> Option<ServerHello> {
        lock(&self.inner.state).hello.clone()
    }

    /// The live Session route selected by the server, if any.
    pub fn attachment(&self) -> Option<SessionTarget> {
        lock(&self.inner.state).attachment.clone()
    }

    pub async fn connect(&self) -> Result<ServerHello, ClientError> {
        self.inner.connect().await
    }

    /// Connect again after a disconnection. Requests are never replayed.
    pub async fn reconnect(&self) -> Result<ServerHello, ClientError> {
        self.connect().await
    }

    pub fn disconnect(&self, reason: &str) {
        self.inner.update(|state, effects| state.fail_and_close(ClientError::disconnected_with(reason), effects));
    }

    pub fn on_connection_state_change(&self, listener: ConnectionStateListener) -> Result<ListenerGuard, ClientError> {
        self.add_listener(
            |state, id| state.listeners.state.push((id, listener)),
            |state, id| state.listeners.state.retain(|(existing, _)| *existing != id),
        )
    }

    pub fn on_attachment_change(&self, listener: AttachmentChangeListener) -> Result<ListenerGuard, ClientError> {
        self.add_listener(
            |state, id| state.listeners.attachment.push((id, listener)),
            |state, id| state.listeners.attachment.retain(|(existing, _)| *existing != id),
        )
    }

    fn add_listener(
        &self,
        add: impl FnOnce(&mut State, u64),
        remove: impl FnOnce(&mut State, u64) + Send + 'static,
    ) -> Result<ListenerGuard, ClientError> {
        let mut state = lock(&self.inner.state);
        if state.disposed {
            return Err(ClientError::Disposed);
        }
        state.listeners.next += 1;
        let id = state.listeners.next;
        add(&mut state, id);
        let weak = Arc::downgrade(&self.inner);
        let remove: Box<dyn FnOnce() + Send> = Box::new(move || {
            if let Some(inner) = weak.upgrade() {
                remove(&mut lock(&inner.state), id);
            }
        });
        Ok(ListenerGuard { remove: Mutex::new(Some(remove)) })
    }

    /// Invoke one low-level protocol call against an explicit routed target.
    pub async fn request(
        &self,
        target: RpcTarget,
        call: ServiceCall,
        signal: Option<AbortSignal>,
    ) -> Result<Option<Json>, ClientError> {
        match self.inner.request(target, call, signal, None).await? {
            Reply::Value(value) => Ok(value),
            Reply::Snapshot(_) => Err(ClientError::validation("Unexpected subscription snapshot")),
        }
    }

    pub async fn service_catalogue(
        &self,
        target: RpcTarget,
        signal: Option<AbortSignal>,
    ) -> Result<Vec<ServiceCatalogueEntry>, ClientError> {
        let result = self.request(target, create_service_catalogue_call(), signal).await?;
        parse_service_catalogue(&result.unwrap_or(Json::Null)).map_err(|error| {
            let error = ClientError::validation(error.to_string());
            self.inner.update(|state, effects| state.fail_and_close(error.clone(), effects));
            error
        })
    }

    /// Reject pending requests, close the connection, and drop every listener. Idempotent.
    pub fn dispose(&self) {
        self.inner.update(|state, effects| {
            if state.disposed {
                return;
            }
            state.disposed = true;
            state.reject_pending(&ClientError::Disposed, effects);
            state.fail_and_close(ClientError::Disposed, effects);
            state.hello = None;
            state.set_attachment(None, effects);
        });
        let mut state = lock(&self.inner.state);
        state.listeners.state.clear();
        state.listeners.attachment.clear();
        state.services.clear();
    }

    pub(crate) fn target_is_current(&self, target: &RpcTarget) -> bool {
        let state = lock(&self.inner.state);
        match target {
            RpcTarget::Server { server_id } => state.hello.as_ref().is_some_and(|hello| &hello.server_id == server_id),
            RpcTarget::Session(target) => state.attachment.as_ref() == Some(target),
        }
    }
}

impl Inner {
    pub(crate) async fn request(
        &self,
        target: RpcTarget,
        call: ServiceCall,
        signal: Option<AbortSignal>,
        subscription: Option<Arc<ActiveService>>,
    ) -> Result<Reply, ClientError> {
        let (sender, receiver) = oneshot::channel();
        let (id, transport, frame) = {
            let mut state = lock(&self.state);
            if state.disposed {
                return Err(ClientError::Disposed);
            }
            let Lifecycle::Connected { transport, .. } = &state.lifecycle else {
                return Err(ClientError::disconnected());
            };
            let transport = transport.clone();
            if let Some(reason) = signal.as_ref().and_then(AbortSignal::reason) {
                return Err(ClientError::Aborted(reason));
            }
            state.request_sequence += 1;
            let id = format!("request-{}", state.request_sequence);
            let call = call.to_json();
            parse_service_call(&call).map_err(|error| ClientError::Type(error.to_string()))?;
            let request = RequestEnvelope { id: id.clone(), target: target.clone(), call };
            let frame = encode_client_message(&ClientMessage::Request(request), &self.frame_options())?;
            state.pending.insert(id.clone(), Pending { sender: Some(sender), subscription, abort: None });
            (id, transport, frame)
        };
        self.send(transport, frame);
        if let Some(signal) = signal {
            let weak = self.me.clone();
            let (request_id, request_target) = (id.clone(), target);
            let listener = signal.add_listener(move |reason| {
                if let Some(inner) = weak.upgrade() {
                    inner.abort_request(&request_id, request_target, reason.clone());
                }
            });
            if let Some(pending) = lock(&self.state).pending.get_mut(&id) {
                pending.abort = Some((signal, listener));
            }
        }
        receiver.await.unwrap_or_else(|_| Err(ClientError::disconnected()))
    }

    /// Queue a frame; a failed delivery disconnects the transport that carried it.
    fn send(&self, transport: Arc<dyn crate::transport::ByteTransport>, frame: Vec<u8>) {
        let sending = transport.send(frame);
        let weak = self.me.clone();
        tokio::spawn(async move {
            if let Err(error) = sending.await
                && let Some(inner) = weak.upgrade()
            {
                inner.update(|state, effects| {
                    let current = state.lifecycle.transport();
                    if current.is_some_and(|current| Arc::ptr_eq(&current, &transport)) {
                        state.fail_and_close(error.to_disconnected(), effects);
                    }
                });
            }
        });
    }

    fn abort_request(&self, id: &str, target: RpcTarget, reason: pi_chord::ChordError) {
        let mut effects = Vec::new();
        let transport = {
            let mut state = lock(&self.state);
            let Some(pending) = state.pending.get_mut(id) else { return };
            if let Some(sender) = pending.sender.take() {
                effects.push(Effect::Reply(sender, Err(ClientError::Aborted(reason))));
            }
            match &state.lifecycle {
                Lifecycle::Connected { transport, .. } => Some(transport.clone()),
                _ => None,
            }
        };
        self.run(effects);
        let Some(transport) = transport else { return };
        let cancel = ClientMessage::Cancel(CancelEnvelope { id: id.to_string(), target });
        match encode_client_message(&cancel, &self.frame_options()) {
            Ok(frame) => self.send(transport, frame),
            Err(error) => self.update(|state, effects| state.fail_and_close(error.into(), effects)),
        }
    }
}
