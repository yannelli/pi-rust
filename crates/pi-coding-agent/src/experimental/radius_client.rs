//! The client side of upstream `radius-relay.ts`: a raw authenticated WebSocket as the client byte transport,
//! and the reconnect of an established Radius client.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use pi_chord::{ChordError, ChordResult};
use pi_client::{ByteTransport, ByteTransportFactory, ByteTransportHandlers, Client, ClientError, ConnectionState};
use tokio_util::sync::CancellationToken;

use super::radius_auth::RadiusRelayAuthResolver;
use super::radius_socket::{
    OrderedWebSocketWriter, RADIUS_RELAY_CLIENT_SUBPROTOCOL, RadiusRelayWebSocket, RadiusRelayWebSocketFactory,
    RelayFrame, RelaySocket, RelaySocketEvent, close_web_socket, default_web_socket_factory,
    open_radius_relay_web_socket, relay_web_socket_url, web_socket_error,
};

const CLIENT_RETRY_INITIAL: Duration = Duration::from_millis(1_000);
const CLIENT_RETRY_MAX: Duration = Duration::from_millis(30_000);
const LOCAL_TRANSPORT_ERROR_CLOSE_CODE: u16 = 4001;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn client_error(error: ChordError) -> ClientError {
    ClientError::Other(error.to_string())
}

pub fn create_radius_client_transport_factory(
    server_id: String,
    auth: Arc<RadiusRelayAuthResolver>,
    web_socket_factory: Option<RadiusRelayWebSocketFactory>,
) -> ByteTransportFactory {
    Arc::new(move |handlers| {
        let (server_id, auth, factory) = (server_id.clone(), auth.clone(), web_socket_factory.clone());
        Box::pin(async move {
            let auth = auth.resolve(true, None).await.map_err(client_error)?;
            let auth = auth.ok_or_else(|| ClientError::Other("Radius authentication is required".into()))?;
            let url = relay_web_socket_url(&auth.gateway, &server_id).map_err(client_error)?;
            let factory = factory.unwrap_or_else(default_web_socket_factory);
            let protocol = RADIUS_RELAY_CLIENT_SUBPROTOCOL;
            let socket = open_radius_relay_web_socket(url, protocol, &auth.token, &factory, None).await;
            let transport = RadiusClientByteTransport::start(socket.map_err(client_error)?, handlers);
            Ok(transport as Arc<dyn ByteTransport>)
        })
    })
}

struct RadiusClientByteTransport {
    socket: Arc<dyn RadiusRelayWebSocket>,
    writer: OrderedWebSocketWriter,
    closed: AtomicBool,
}

impl RadiusClientByteTransport {
    fn start((socket, mut events): RelaySocket, handlers: ByteTransportHandlers) -> Arc<Self> {
        let writer = OrderedWebSocketWriter::new(socket.clone());
        let transport = Arc::new(Self { socket, writer, closed: AtomicBool::new(false) });
        let reader = transport.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if reader.closed.load(Ordering::SeqCst) {
                    return;
                }
                match event {
                    RelaySocketEvent::Message(RelayFrame::Binary(bytes)) => handlers.on_data(&bytes),
                    RelaySocketEvent::Message(RelayFrame::Text(_)) => {
                        let error = ChordError::error("Radius relay client received a non-binary message");
                        return reader.fail(&handlers, error);
                    }
                    RelaySocketEvent::Close { .. } => {
                        if reader.mark_closed() {
                            handlers.on_close();
                        }
                        return;
                    }
                    RelaySocketEvent::Error { message } => {
                        return reader.fail(&handlers, web_socket_error(message.as_deref()));
                    }
                    RelaySocketEvent::Open { .. } => {}
                }
            }
        });
        transport
    }

    fn fail(&self, handlers: &ByteTransportHandlers, error: ChordError) {
        if !self.mark_closed() {
            return;
        }
        close_web_socket(self.socket.as_ref(), LOCAL_TRANSPORT_ERROR_CLOSE_CODE, "Radius relay transport error");
        handlers.on_error(client_error(error));
    }

    fn mark_closed(&self) -> bool {
        if self.closed.swap(true, Ordering::SeqCst) {
            return false;
        }
        self.writer.close();
        true
    }
}

impl ByteTransport for RadiusClientByteTransport {
    fn send(&self, chunk: Vec<u8>) -> BoxFuture<'static, Result<(), ClientError>> {
        if self.closed.load(Ordering::SeqCst) {
            return Box::pin(async { Err(ClientError::Other("Radius relay client is closed".into())) });
        }
        let sent = self.writer.send(RelayFrame::Binary(chunk));
        Box::pin(async move { sent.await.map_err(client_error) })
    }

    fn close(&self) {
        if self.mark_closed() {
            close_web_socket(self.socket.as_ref(), 1000, "Pi client closed");
        }
    }
}

pub type Remove = Box<dyn FnOnce() + Send>;

/// The part of a `Client` the reconnect uses (upstream `RadiusReconnectClient`).
pub trait RadiusReconnectClient: Send + Sync {
    fn attachment_session_id(&self) -> Option<String>;
    fn connected(&self) -> bool;
    fn connection_state(&self) -> ConnectionState;
    fn disconnect(&self, reason: &str);
    fn reconnect(&self) -> BoxFuture<'static, Result<(), ClientError>>;
    fn on_attachment_change(&self, listener: Arc<dyn Fn(Option<String>) + Send + Sync>) -> Remove;
    fn on_connection_state_change(&self, listener: Arc<dyn Fn(ConnectionState) + Send + Sync>) -> Remove;
}

impl RadiusReconnectClient for Client {
    fn attachment_session_id(&self) -> Option<String> {
        self.attachment().map(|target| target.session_id)
    }
    fn connected(&self) -> bool {
        Client::connected(self)
    }
    fn connection_state(&self) -> ConnectionState {
        Client::connection_state(self)
    }
    fn disconnect(&self, reason: &str) {
        Client::disconnect(self, reason)
    }
    fn reconnect(&self) -> BoxFuture<'static, Result<(), ClientError>> {
        let client = self.clone();
        Box::pin(async move { Client::reconnect(&client).await.map(drop) })
    }
    fn on_attachment_change(&self, listener: Arc<dyn Fn(Option<String>) + Send + Sync>) -> Remove {
        let guard = Client::on_attachment_change(
            self,
            Arc::new(move |target| listener(target.map(|target| target.session_id.clone()))),
        );
        Box::new(move || drop(guard))
    }
    fn on_connection_state_change(&self, listener: Arc<dyn Fn(ConnectionState) + Send + Sync>) -> Remove {
        let guard = Client::on_connection_state_change(self, Arc::new(move |change| listener(change.state)));
        Box::new(move || drop(guard))
    }
}

pub type Reattach = Arc<dyn Fn(String) -> BoxFuture<'static, ChordResult<()>> + Send + Sync>;

struct ReconnectInner {
    client: Arc<dyn RadiusReconnectClient>,
    reattach: Reattach,
    cancel: CancellationToken,
    desired: Mutex<Option<String>>,
    reconnecting: Mutex<Option<tokio::task::JoinHandle<()>>>,
    disposed: AtomicBool,
}

/// Reconnect one established Radius client and restore its last selected Session.
pub struct RadiusClientReconnect {
    inner: Arc<ReconnectInner>,
    removers: Mutex<Vec<Remove>>,
}

impl RadiusClientReconnect {
    pub fn new(client: Arc<dyn RadiusReconnectClient>, reattach: Reattach) -> Self {
        let inner = Arc::new(ReconnectInner {
            desired: Mutex::new(client.attachment_session_id()),
            client: client.clone(),
            reattach,
            cancel: CancellationToken::new(),
            reconnecting: Mutex::default(),
            disposed: AtomicBool::new(false),
        });
        let attachments = Arc::downgrade(&inner);
        let remove_attachment = client.on_attachment_change(Arc::new(move |session_id| {
            let Some(inner) = attachments.upgrade() else { return };
            if session_id.is_some() || inner.client.connected() {
                *lock(&inner.desired) = session_id;
            }
        }));
        let states = Arc::downgrade(&inner);
        let remove_state = client.on_connection_state_change(Arc::new(move |state| {
            let Some(inner) = states.upgrade() else { return };
            if state == ConnectionState::Disconnected && !inner.disposed.load(Ordering::SeqCst) {
                inner.start_reconnect();
            }
        }));
        Self { inner, removers: Mutex::new(vec![remove_state, remove_attachment]) }
    }

    pub async fn dispose(&self) {
        if self.inner.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        for remove in lock(&self.removers).drain(..) {
            remove();
        }
        self.inner.cancel.cancel();
        if self.inner.client.connection_state() != ConnectionState::Disconnected {
            self.inner.client.disconnect("Radius reconnect stopped");
        }
        let running = lock(&self.inner.reconnecting).take();
        if let Some(running) = running {
            let _ = running.await;
        }
    }
}

impl ReconnectInner {
    fn start_reconnect(self: &Arc<Self>) {
        let mut reconnecting = lock(&self.reconnecting);
        if reconnecting.as_ref().is_some_and(|running| !running.is_finished()) {
            return;
        }
        let inner = self.clone();
        *reconnecting = Some(tokio::spawn(async move { inner.run_reconnect().await }));
    }

    async fn run_reconnect(self: Arc<Self>) {
        let mut retry = CLIENT_RETRY_INITIAL;
        while !self.disposed.load(Ordering::SeqCst) && !self.client.connected() {
            tokio::select! {
                () = tokio::time::sleep(retry) => {}
                () = self.cancel.cancelled() => return,
            }
            let result = async {
                self.client.reconnect().await.map_err(|error| ChordError::error(error.to_string()))?;
                let session_id = lock(&self.desired).clone();
                match session_id {
                    Some(session_id) => (self.reattach)(session_id).await,
                    None => Ok(()),
                }
            }
            .await;
            let Err(error) = result else { return };
            if self.disposed.load(Ordering::SeqCst) || self.cancel.is_cancelled() {
                return;
            }
            if self.client.connected() {
                self.client.disconnect(&error.to_string());
            }
            retry = (retry * 2).min(CLIENT_RETRY_MAX);
        }
    }
}
