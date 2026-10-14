//! Remote service sources over one presentation client (upstream `services/connection.ts`): the server
//! scope follows the connection, and the Session scope follows the client's attachment.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use futures::future::BoxFuture;
use pi_chord::services::{
    ObserveHandler, RemoteServiceBinding, RemoteServiceBindingOptions, RemoteServiceSource,
    RemoteServiceSourceOpenOptions, RemoteServiceTransport, RemoteServices, ServiceCatalogueEntry, ServiceHandle,
    ServiceRef, create_remote_service_binding,
};
use pi_chord::state::{ErrorReporter, MutableReplicatedState};
use pi_chord::{BACKGROUND_CONTEXT, ChordError, ChordResult, Context, Json, Unsubscribe};
use pi_client::{Client, ConnectionState, ListenerGuard, create_client_service_transport};
use pi_protocol::{RpcTarget, SessionTarget};
use serde::{Deserialize, Serialize};

use super::codec::{decode_result, encode};
use crate::experimental::process::lock;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum ServerConnectionState {
    Connecting {
        attempt: u64,
    },
    Connected {
        since: String,
    },
    #[serde(rename_all = "camelCase")]
    Disconnected {
        since: String,
        reason: String,
        retry_at: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum SessionAttachmentState {
    Detached,
    #[serde(rename_all = "camelCase")]
    Attaching {
        session_id: String,
    },
    #[serde(rename_all = "camelCase")]
    Attached {
        session_id: String,
    },
    #[serde(rename_all = "camelCase")]
    Degraded {
        session_id: String,
    },
}

type Activate = Arc<dyn Fn(Context) -> BoxFuture<'static, ChordResult<()>> + Send + Sync>;

pub(super) struct RoutedServiceBinding {
    services: RemoteServiceBinding,
    get_bound: Box<dyn Fn() -> bool + Send + Sync>,
    on_activate: Option<Activate>,
    remove: Box<dyn Fn() + Send + Sync>,
    activated: AtomicBool,
    activation_complete: AtomicBool,
}

impl RoutedServiceBinding {
    async fn service_ready(&self, context: &Context) -> ChordResult<()> {
        self.services.ready(context).await
    }

    async fn update_bound(&self, bound: bool, context: &Context) -> ChordResult<()> {
        if self.activated.load(Ordering::SeqCst) { self.services.rebind(bound, context).await } else { Ok(()) }
    }
}

#[async_trait]
impl RemoteServices for RoutedServiceBinding {
    fn use_service(&self, service: &ServiceRef) -> ChordResult<ServiceHandle> {
        self.services.use_service(service)
    }

    fn observe(&self, service: &ServiceRef, handler: ObserveHandler) -> ChordResult<Unsubscribe> {
        self.services.observe(service, handler)
    }

    async fn ready(&self, context: &Context) -> ChordResult<()> {
        if !self.activated.swap(true, Ordering::SeqCst) {
            self.services.rebind((self.get_bound)(), context).await?;
        }
        self.service_ready(context).await?;
        if !self.activation_complete.load(Ordering::SeqCst) {
            if let Some(activate) = &self.on_activate {
                activate(context.clone()).await?;
            }
            self.activation_complete.store(true, Ordering::SeqCst);
        }
        Ok(())
    }

    async fn dispose(&self, context: &Context) -> ChordResult<()> {
        (self.remove)();
        self.services.dispose(context).await
    }
}

type Bindings = Arc<Mutex<Vec<Arc<RoutedServiceBinding>>>>;

pub(super) fn open_binding(
    bindings: &Bindings,
    transport: &Arc<dyn RemoteServiceTransport>,
    options: RemoteServiceSourceOpenOptions,
    get_bound: Box<dyn Fn() -> bool + Send + Sync>,
    on_activate: Option<Activate>,
) -> ChordResult<Arc<RoutedServiceBinding>> {
    let mut binding_options = RemoteServiceBindingOptions::new(options.services, transport.clone());
    binding_options.bound = false;
    binding_options.assert_access = Some(options.assert_access);
    binding_options.on_error = Some(options.on_error);
    let services = create_remote_service_binding(binding_options)?;
    let binding = Arc::new_cyclic(|me: &Weak<RoutedServiceBinding>| {
        let (me, list) = (me.clone(), Arc::downgrade(bindings));
        let remove = Box::new(move || {
            if let Some(list) = list.upgrade() {
                lock(&list).retain(|binding| !std::ptr::eq(Arc::as_ptr(binding), me.as_ptr()));
            }
        });
        RoutedServiceBinding {
            services,
            get_bound,
            on_activate,
            remove,
            activated: AtomicBool::new(false),
            activation_complete: AtomicBool::new(false),
        }
    });
    lock(bindings).push(binding.clone());
    Ok(binding)
}

pub(super) async fn rebind_all(bindings: &Bindings, bound: bool, context: &Context, message: &str) -> ChordResult<()> {
    let list: Vec<Arc<RoutedServiceBinding>> = lock(bindings).clone();
    let results = futures::future::join_all(list.iter().map(|binding| binding.update_bound(bound, context))).await;
    ChordError::collect(results.into_iter().filter_map(Result::err).collect(), message)
}

pub(super) async fn dispose_all(bindings: &Bindings, context: &Context, message: &str) -> ChordResult<()> {
    let list: Vec<Arc<RoutedServiceBinding>> = std::mem::take(&mut *lock(bindings));
    let results = futures::future::join_all(list.iter().map(|binding| binding.services.dispose(context))).await;
    ChordError::collect(results.into_iter().filter_map(Result::err).collect(), message)
}

fn replace<T: Serialize>(state: &MutableReplicatedState, value: &T, context: &Context) {
    if let Ok(value) = encode(value) {
        let _ = state.replace(context, value);
    }
}

fn now_iso() -> String {
    jiff::Timestamp::now().strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

fn connection_state(client: &Client, attempt: u64, error: Option<String>) -> ServerConnectionState {
    match client.connection_state() {
        ConnectionState::Connecting => ServerConnectionState::Connecting { attempt },
        ConnectionState::Connected => ServerConnectionState::Connected { since: now_iso() },
        ConnectionState::Disconnected => ServerConnectionState::Disconnected {
            since: now_iso(),
            reason: error.unwrap_or_else(|| "Client is disconnected".into()),
            retry_at: None,
        },
    }
}

/// The server-scoped remote service source for one presentation client.
pub struct ServerServiceSource {
    client: Client,
    transport: Arc<dyn RemoteServiceTransport>,
    bindings: Bindings,
    pub connection: MutableReplicatedState,
    guard: Mutex<Option<ListenerGuard>>,
    transition: Arc<tokio::sync::Mutex<()>>,
    disposed: AtomicBool,
}

impl ServerServiceSource {
    pub fn new(client: &Client, on_error: Option<ErrorReporter>) -> ChordResult<Arc<Self>> {
        let on_error = on_error.unwrap_or_else(|| Arc::new(|_| {}));
        let target_client = client.clone();
        let transport = create_client_service_transport(
            client,
            Arc::new(move || Some(RpcTarget::Server { server_id: target_client.server_id().to_string() })),
        );
        let attempt = Arc::new(AtomicU64::new(u64::from(client.connection_state() == ConnectionState::Connecting)));
        let connection =
            MutableReplicatedState::new(encode(&connection_state(client, attempt.load(Ordering::SeqCst), None))?);
        let bindings: Bindings = Arc::default();
        let transition = Arc::new(tokio::sync::Mutex::new(()));
        let (state, listener_client, listener_bindings, listener_transition) =
            (connection.clone(), client.clone(), bindings.clone(), transition.clone());
        let guard = client
            .on_connection_state_change(Arc::new(move |change| {
                if change.state == ConnectionState::Connecting {
                    attempt.fetch_add(1, Ordering::SeqCst);
                }
                let error = change.error.as_ref().map(ToString::to_string);
                replace(
                    &state,
                    &connection_state(&listener_client, attempt.load(Ordering::SeqCst), error),
                    &BACKGROUND_CONTEXT,
                );
                let (bindings, transition, on_error) =
                    (listener_bindings.clone(), listener_transition.clone(), on_error.clone());
                let connected = change.state == ConnectionState::Connected;
                tokio::spawn(async move {
                    let _serialized = transition.lock().await;
                    if let Err(error) =
                        rebind_all(&bindings, connected, &BACKGROUND_CONTEXT, "Failed to rebind server services").await
                    {
                        on_error(error);
                    }
                });
            }))
            .map_err(ChordError::other)?;
        Ok(Arc::new(Self {
            client: client.clone(),
            transport,
            bindings,
            connection,
            guard: Mutex::new(Some(guard)),
            transition,
            disposed: AtomicBool::new(false),
        }))
    }

    pub fn connection_state(&self) -> ChordResult<ServerConnectionState> {
        decode_result(Some(self.connection.value()))
    }

    pub async fn dispose(&self, context: &Context) -> ChordResult<()> {
        if self.disposed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        if let Some(guard) = lock(&self.guard).take() {
            guard.remove();
        }
        drop(self.transition.lock().await);
        dispose_all(&self.bindings, context, "Failed to dispose server service source").await
    }
}

#[async_trait]
impl RemoteServiceSource for ServerServiceSource {
    fn accepts_unavailable_services(&self) -> bool {
        false
    }

    async fn catalogue(&self, context: &Context) -> ChordResult<Vec<ServiceCatalogueEntry>> {
        let target = RpcTarget::Server { server_id: self.client.server_id().to_string() };
        self.client.service_catalogue(target, context.abort_signal()).await.map_err(ChordError::other)
    }

    fn open(&self, options: RemoteServiceSourceOpenOptions) -> ChordResult<Arc<dyn RemoteServices>> {
        if self.disposed.load(Ordering::SeqCst) {
            return Err(ChordError::error("Server service source is disposed"));
        }
        let client = self.client.clone();
        let binding =
            open_binding(&self.bindings, &self.transport, options, Box::new(move || client.connected()), None)?;
        Ok(binding)
    }
}

pub use super::connection_session::SessionServiceSource;

/// Session attachment state shared with the Session source implementation.
pub(super) fn attachment_value(state: &MutableReplicatedState) -> Option<SessionAttachmentState> {
    decode_result(Some(state.value())).ok()
}

pub(super) fn publish_attachment(state: &MutableReplicatedState, value: &SessionAttachmentState, context: &Context) {
    replace(state, value, context);
}

pub(super) fn same_attachment(left: Option<&SessionTarget>, right: Option<&SessionTarget>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => {
            left.server_id == right.server_id
                && left.session_id == right.session_id
                && left.attachment_id == right.attachment_id
        }
        (None, None) => true,
        _ => false,
    }
}

pub(super) type RoutedBindings = Bindings;
pub(super) type RoutedActivate = Activate;

impl RoutedServiceBinding {
    pub(super) async fn ready_service(&self, context: &Context) -> ChordResult<()> {
        self.service_ready(context).await
    }
}

/// A JSON value helper for tests and presentations reading replicated state.
pub fn state_value<T: serde::de::DeserializeOwned>(value: Json) -> ChordResult<T> {
    decode_result(Some(value))
}
