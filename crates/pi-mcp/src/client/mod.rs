//! MCP client: initialization, requests with timeouts and progress, and notifications.

mod dispatch;
mod lists;
mod validate;

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::{Map, Value, json};
use tokio::sync::{Notify, oneshot};
use tokio_util::sync::CancellationToken;

use crate::error::McpError;
use crate::jsonrpc::{JsonRpcId, JsonRpcMessage};
use crate::transport::{SharedTransport, TransportSink};
use crate::types::{
    Implementation, InitializeResult, JsonObject, LATEST_PROTOCOL_VERSION, ProgressNotification, Root,
    SUPPORTED_PROTOCOL_VERSIONS, ServerCapabilities,
};
use dispatch::{Outcome, PendingGuard, wait_for_response};

pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientState {
    Idle,
    Connecting,
    Connected,
    Closed,
}

impl fmt::Display for ClientState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Idle => "idle",
            Self::Connecting => "connecting",
            Self::Connected => "connected",
            Self::Closed => "closed",
        })
    }
}

pub type ProgressFn = Arc<dyn Fn(&ProgressNotification) + Send + Sync>;
pub type NotificationListener = Arc<dyn Fn(Option<&Value>) + Send + Sync>;
pub type ErrorListener = Arc<dyn Fn(&McpError) + Send + Sync>;
pub type CloseListener = Arc<dyn Fn() + Send + Sync>;
/// Answers a request from the server. The token is cancelled by `notifications/cancelled`.
pub type RequestHandler =
    Arc<dyn Fn(Option<Value>, CancellationToken) -> BoxFuture<'static, Result<Value, McpError>> + Send + Sync>;

#[derive(Clone, Debug, Default)]
pub struct McpClientOptions {
    pub name: String,
    pub version: String,
    pub title: Option<String>,
    /// Client capabilities sent in `initialize`.
    pub capabilities: JsonObject,
    pub protocol_version: Option<String>,
    /// Default: 30 s. Zero disables the timeout.
    pub request_timeout: Option<Duration>,
    /// Answered to `roots/list`; also announces the `roots` capability.
    pub roots: Option<Vec<Root>>,
}

impl McpClientOptions {
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self { name: name.into(), version: version.into(), ..Self::default() }
    }
}

#[derive(Clone, Default)]
pub struct RequestOptions {
    pub cancel: Option<CancellationToken>,
    /// Overrides the client's timeout. Zero disables it. Progress notifications restart it.
    pub timeout: Option<Duration>,
    pub on_progress: Option<ProgressFn>,
}

struct Pending {
    sender: oneshot::Sender<Result<Value, McpError>>,
    progress: Option<(Arc<Notify>, ProgressFn)>,
}

#[derive(Default)]
struct Listeners {
    next: u64,
    notifications: HashMap<String, Vec<(u64, NotificationListener)>>,
    errors: Vec<(u64, ErrorListener)>,
    closes: Vec<(u64, CloseListener)>,
}

struct Shared {
    state: ClientState,
    transport: Option<SharedTransport>,
    /// Incremented when the transport is detached, so its late events are ignored.
    generation: u64,
    next_id: i64,
    initialize: Option<InitializeResult>,
    pending: HashMap<JsonRpcId, Pending>,
    incoming: HashMap<JsonRpcId, CancellationToken>,
    handlers: HashMap<String, RequestHandler>,
}

struct Inner {
    options: McpClientOptions,
    shared: Mutex<Shared>,
    listeners: Mutex<Listeners>,
}

/// An MCP client. Clones share one connection.
#[derive(Clone)]
pub struct McpClient {
    inner: Arc<Inner>,
}

impl McpClient {
    pub fn new(options: McpClientOptions) -> Self {
        let mut handlers: HashMap<String, RequestHandler> = HashMap::new();
        handlers.insert("ping".into(), Arc::new(|_, _| Box::pin(async { Ok(json!({})) })));
        if let Some(roots) = options.roots.clone() {
            let roots = json!({ "roots": roots });
            handlers.insert(
                "roots/list".into(),
                Arc::new(move |_, _| {
                    let roots = roots.clone();
                    Box::pin(async move { Ok(roots) })
                }),
            );
        }
        let shared = Shared {
            state: ClientState::Idle,
            transport: None,
            generation: 0,
            next_id: 1,
            initialize: None,
            pending: HashMap::new(),
            incoming: HashMap::new(),
            handlers,
        };
        let inner = Inner { options, shared: Mutex::new(shared), listeners: Mutex::default() };
        Self { inner: Arc::new(inner) }
    }

    pub fn options(&self) -> &McpClientOptions {
        &self.inner.options
    }

    pub fn connection_state(&self) -> ClientState {
        self.inner.shared().state
    }

    pub fn is_connected(&self) -> bool {
        self.connection_state() == ClientState::Connected
    }

    pub fn server_info(&self) -> Option<Implementation> {
        self.inner.shared().initialize.as_ref().map(|result| result.server_info.clone())
    }

    pub fn server_capabilities(&self) -> Option<ServerCapabilities> {
        self.inner.shared().initialize.as_ref().map(|result| result.capabilities.clone())
    }

    pub fn instructions(&self) -> Option<String> {
        self.inner.shared().initialize.as_ref().and_then(|result| result.instructions.clone())
    }

    pub fn protocol_version(&self) -> Option<String> {
        self.inner.shared().initialize.as_ref().map(|result| result.protocol_version.clone())
    }

    /// Whether two handles share one connection.
    pub fn same(&self, other: &McpClient) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub async fn connect(&self, transport: SharedTransport) -> Result<InitializeResult, McpError> {
        let generation = {
            let mut shared = self.inner.shared();
            if shared.state != ClientState::Idle {
                return Err(McpError::other(format!("Cannot connect MCP client in {} state", shared.state)));
            }
            shared.state = ClientState::Connecting;
            shared.transport = Some(transport.clone());
            shared.generation
        };
        let (sink, events) = TransportSink::channel();
        tokio::spawn(dispatch::run(Arc::downgrade(&self.inner), generation, events));
        let result = self.initialize(&transport, sink).await;
        if result.is_err() {
            self.close().await;
        }
        result
    }

    async fn initialize(&self, transport: &SharedTransport, sink: TransportSink) -> Result<InitializeResult, McpError> {
        transport.start(sink).await?;
        let options = &self.inner.options;
        let mut capabilities = options.capabilities.clone();
        if options.roots.is_some() && !capabilities.contains_key("roots") {
            capabilities.insert("roots".into(), json!({}));
        }
        let mut client_info = json!({ "name": options.name, "version": options.version });
        if let Some(title) = &options.title {
            client_info["title"] = json!(title);
        }
        let params = json!({
            "protocolVersion": options.protocol_version.as_deref().unwrap_or(LATEST_PROTOCOL_VERSION),
            "capabilities": capabilities,
            "clientInfo": client_info,
        });
        let value = self.request_internal("initialize", Some(params), RequestOptions::default(), true).await?;
        let result = validate::initialize_result(value)?;
        if !SUPPORTED_PROTOCOL_VERSIONS.contains(&result.protocol_version.as_str()) {
            return Err(McpError::other(format!(
                "MCP server selected unsupported protocol version {}",
                result.protocol_version
            )));
        }
        self.inner.shared().initialize = Some(result.clone());
        transport.set_protocol_version(&result.protocol_version);
        self.notify_internal("notifications/initialized", None, true).await?;
        let mut shared = self.inner.shared();
        if shared.state != ClientState::Connecting {
            return Err(McpError::closed());
        }
        shared.state = ClientState::Connected;
        Ok(result)
    }

    pub async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        options: RequestOptions,
    ) -> Result<Value, McpError> {
        self.request_internal(method, params, options, false).await
    }

    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), McpError> {
        self.notify_internal(method, params, false).await
    }

    /// Answer requests the server sends with `method`.
    pub fn set_request_handler(&self, method: &str, handler: RequestHandler) {
        self.inner.shared().handlers.insert(method.into(), handler);
    }

    pub fn on_notification(&self, method: &str, listener: impl Fn(Option<&Value>) + Send + Sync + 'static) -> u64 {
        let mut listeners = self.inner.listeners();
        listeners.next += 1;
        let id = listeners.next;
        listeners.notifications.entry(method.into()).or_default().push((id, Arc::new(listener)));
        id
    }

    pub fn on_error(&self, listener: impl Fn(&McpError) + Send + Sync + 'static) -> u64 {
        let mut listeners = self.inner.listeners();
        listeners.next += 1;
        let id = listeners.next;
        listeners.errors.push((id, Arc::new(listener)));
        id
    }

    /// Called once when the connection closes, whether the transport dropped or `close` was called.
    pub fn on_close(&self, listener: impl Fn() + Send + Sync + 'static) -> u64 {
        let mut listeners = self.inner.listeners();
        listeners.next += 1;
        let id = listeners.next;
        listeners.closes.push((id, Arc::new(listener)));
        id
    }

    pub fn remove_listener(&self, id: u64) {
        let mut listeners = self.inner.listeners();
        for entries in listeners.notifications.values_mut() {
            entries.retain(|(entry, _)| *entry != id);
        }
        listeners.errors.retain(|(entry, _)| *entry != id);
        listeners.closes.retain(|(entry, _)| *entry != id);
    }

    /// Fail pending requests, detach the transport, and close it.
    pub async fn close(&self) {
        let transport = {
            let mut shared = self.inner.shared();
            shared.generation += 1;
            shared.transport.take()
        };
        self.inner.mark_closed(McpError::closed());
        if let Some(transport) = transport {
            transport.close().await;
        }
    }

    fn require_transport(&self, allow_connecting: bool) -> Result<SharedTransport, McpError> {
        let shared = self.inner.shared();
        let usable =
            shared.state == ClientState::Connected || (allow_connecting && shared.state == ClientState::Connecting);
        match (&shared.transport, usable) {
            (Some(transport), true) => Ok(transport.clone()),
            _ => Err(McpError::ConnectionClosed(format!("MCP client is {}", shared.state))),
        }
    }

    async fn notify_internal(
        &self,
        method: &str,
        params: Option<Value>,
        allow_connecting: bool,
    ) -> Result<(), McpError> {
        let transport = self.require_transport(allow_connecting)?;
        transport.send(&JsonRpcMessage::notification(method, params)).await
    }

    async fn request_internal(
        &self,
        method: &str,
        params: Option<Value>,
        options: RequestOptions,
        allow_connecting: bool,
    ) -> Result<Value, McpError> {
        let transport = self.require_transport(allow_connecting)?;
        let cancel = options.cancel.clone().unwrap_or_default();
        if cancel.is_cancelled() {
            return Err(McpError::Aborted);
        }
        let notify = Arc::new(Notify::new());
        let (sender, receiver) = oneshot::channel();
        let id = {
            let mut shared = self.inner.shared();
            let id = JsonRpcId::from(shared.next_id);
            shared.next_id += 1;
            let progress = options.on_progress.clone().map(|on_progress| (notify.clone(), on_progress));
            shared.pending.insert(id.clone(), Pending { sender, progress });
            id
        };
        let params = match (&options.on_progress, params) {
            (None, params) => params,
            (Some(_), params) => {
                let mut params = match params {
                    Some(Value::Object(params)) => params,
                    _ => Map::new(),
                };
                let mut meta = params.get("_meta").and_then(Value::as_object).cloned().unwrap_or_default();
                meta.insert("progressToken".into(), id.to_value());
                params.insert("_meta".into(), Value::Object(meta));
                Some(Value::Object(params))
            }
        };
        let message = JsonRpcMessage::request(id.clone(), method, params);
        let timeout = options.timeout.or(self.inner.options.request_timeout).unwrap_or(DEFAULT_REQUEST_TIMEOUT);
        // The spec forbids cancelling `initialize`.
        let cancellable = method != "initialize";
        let guard = PendingGuard::new(&self.inner, id.clone());
        let outcome = wait_for_response(&transport, &message, receiver, &notify, timeout, &cancel).await;
        guard.disarm();
        match outcome {
            Outcome::Response(result) => result,
            Outcome::SendFailed(error) => {
                self.inner.cancel_pending(&id, error.clone(), None);
                Err(error)
            }
            Outcome::TimedOut => {
                let error = McpError::Timeout(timeout.as_millis() as u64);
                self.inner.cancel_pending(&id, error.clone(), cancellable.then_some("Request timed out"));
                Err(error)
            }
            Outcome::Aborted => {
                self.inner.cancel_pending(&id, McpError::Aborted, cancellable.then_some("Aborted"));
                Err(McpError::Aborted)
            }
        }
    }
}
