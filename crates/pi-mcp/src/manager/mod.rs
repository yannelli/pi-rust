//! The MCP integration of a session, without its UI: servers from `mcp.json` connect in the
//! background and their tools register as `mcp__<server>__<tool>`. Problems found at startup are
//! reported once through `notify`.

mod actions;
mod registered;
mod registry;
mod status;
mod waits;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use tokio_util::sync::CancellationToken;

use crate::config::{LoadedMcpConfig, McpServerConfigPatch, McpServerEntry, update_mcp_server_config};
use crate::log::McpServerLog;
use crate::oauth::{CallbackServerFactory, LoopbackCallbackFactory};
use crate::registered::McpServerRegistry;
use crate::runtime::{
    ConnectionOptions, McpOAuthCredentialStore, McpServerConnection, ProviderTokenFn, ServerState, TransportFactory,
    default_transport_factory,
};

pub use actions::McpConnectionId;
pub use registry::McpAgentTool;
pub use status::{McpServerStatus, describe_state};
pub use waits::{STILL_CONNECTING_MESSAGE, reaches_server};

pub const CODEMODE_TOOL_NAME: &str = "codemode";
pub const TOOL_SEARCH_TOOL_NAME: &str = "tool_search";
const DEFAULT_STARTUP_WAIT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotifyLevel {
    Info,
    Warning,
    Error,
}

pub type NotifyFn = Arc<dyn Fn(&str, NotifyLevel) + Send + Sync>;
/// Saves a change to the server's config file: its project override when set, else its source.
pub type UpdateConfigFn = Arc<dyn Fn(&McpServerEntry, &McpServerConfigPatch) -> Result<(), String> + Send + Sync>;
type Ready = Shared<BoxFuture<'static, ()>>;
type Listener = Arc<dyn Fn() + Send + Sync>;

/// Subscription ids are unique across managers, so a stale id never removes another listener.
static NEXT_SUBSCRIPTION: AtomicU64 = AtomicU64::new(1);

/// A listener added with [`McpManager::subscribe`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct McpSubscription(pub u64);

pub struct McpManagerOptions {
    /// Where `mcp.json`, `mcp-auth.json`, and `mcp.log` live.
    pub agent_dir: PathBuf,
    /// Default: stdio and streamable HTTP transports built from the server config.
    pub create_transport: TransportFactory,
    /// Default: `mcp-auth.json` in the agent directory.
    pub credentials: Option<McpOAuthCredentialStore>,
    /// Default: `mcp.log` in the agent directory.
    pub log_path: Option<PathBuf>,
    /// Current token of a pi provider, for servers with `auth.provider`.
    pub provider_token: Option<ProviderTokenFn>,
    pub update_config: Option<UpdateConfigFn>,
    /// How long the first prompt waits for servers with `direct` tools. Default: 10 s.
    pub startup_wait: Duration,
    pub notify: Option<NotifyFn>,
    /// Receives OAuth redirects during sign-in. Default: the loopback callback server.
    pub callback_factory: Arc<dyn CallbackServerFactory>,
    pub client_version: String,
    /// Servers extensions register; they connect next to the configured ones while registered.
    pub registry: Option<McpServerRegistry>,
    /// Called after registered servers were added or removed, for example to activate codemode.
    pub on_registered_change: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl McpManagerOptions {
    pub fn new(agent_dir: impl Into<PathBuf>) -> Self {
        Self {
            agent_dir: agent_dir.into(),
            create_transport: default_transport_factory(),
            credentials: None,
            log_path: None,
            provider_token: None,
            update_config: None,
            startup_wait: DEFAULT_STARTUP_WAIT,
            notify: None,
            callback_factory: Arc::new(LoopbackCallbackFactory),
            client_version: env!("CARGO_PKG_VERSION").into(),
            registry: None,
            on_registered_change: None,
        }
    }
}

/// A configured server. Disabled servers have no connection.
struct Server {
    id: u64,
    entry: McpServerEntry,
    connection: Option<McpServerConnection>,
    /// Identifies the current connection try; disabling or replacing it invalidates earlier work.
    attempt: Option<u64>,
    /// Settles when the connection started for the server connected or failed.
    ready: Option<Ready>,
    /// Cleanup of a detached connection; a replacement waits for it before opening a transport.
    closing: Option<Ready>,
    /// Result of the last action that failed.
    message: Option<String>,
    /// For servers extensions registered: the config as registered, to notice registering again.
    registered_config: Option<String>,
}

impl Server {
    fn new(id: u64, entry: McpServerEntry, registered_config: Option<String>) -> Self {
        Self {
            id,
            entry,
            connection: None,
            attempt: None,
            ready: None,
            closing: None,
            message: None,
            registered_config,
        }
    }

    fn registered(&self) -> bool {
        self.registered_config.is_some()
    }

    fn enabled(&self) -> bool {
        self.entry.config.is_enabled()
    }
}

struct State {
    /// Cancelled at shutdown and before the first start.
    session: CancellationToken,
    cwd: PathBuf,
    servers: Vec<Server>,
    next_id: u64,
    config_errors: Vec<String>,
    /// Name and source of the servers from `mcp.json`, which take precedence over registered ones.
    configured: Vec<(String, String)>,
    /// Registered servers that `mcp.json` overrides.
    overridden: Vec<String>,
    registry_listener: Option<u64>,
    project_config: Option<PathBuf>,
    auto_enable_codemode: bool,
    warned_unreachable: bool,
    waited_for_startup: bool,
    pending: Option<Ready>,
    registry: registry::Registry,
    /// Stored tokens of servers waiting for a sign-in, as they were when it was needed.
    tokens_at_sign_in: Vec<(McpServerConnection, String)>,
}

impl State {
    fn server(&self, id: u64) -> Option<&Server> {
        self.servers.iter().find(|server| server.id == id)
    }

    fn server_mut(&mut self, id: u64) -> Option<&mut Server> {
        self.servers.iter_mut().find(|server| server.id == id)
    }

    fn named(&self, name: &str) -> Option<&Server> {
        self.servers.iter().find(|server| server.entry.name == name)
    }
}

pub(crate) struct Inner {
    options: McpManagerOptions,
    credentials: McpOAuthCredentialStore,
    log: Arc<McpServerLog>,
    state: Mutex<State>,
    listeners: Mutex<Vec<(McpSubscription, Listener)>>,
    /// Sign-ins and other actions that outlive their caller; shutdown waits for them.
    background: Mutex<Vec<Ready>>,
}

/// The MCP servers of a session. Clones share it.
#[derive(Clone)]
pub struct McpManager {
    inner: Arc<Inner>,
}

impl McpManager {
    pub fn new(options: McpManagerOptions) -> Self {
        let credentials =
            options.credentials.clone().unwrap_or_else(|| McpOAuthCredentialStore::new(&options.agent_dir));
        let log_path = options.log_path.clone().unwrap_or_else(|| options.agent_dir.join("mcp.log"));
        let session = CancellationToken::new();
        session.cancel();
        let state = State {
            session,
            cwd: std::env::current_dir().unwrap_or_default(),
            servers: Vec::new(),
            next_id: 0,
            config_errors: Vec::new(),
            configured: Vec::new(),
            overridden: Vec::new(),
            registry_listener: None,
            project_config: None,
            auto_enable_codemode: true,
            warned_unreachable: false,
            waited_for_startup: false,
            pending: None,
            registry: registry::Registry::default(),
            tokens_at_sign_in: Vec::new(),
        };
        let inner = Inner {
            options,
            credentials,
            log: Arc::new(McpServerLog::new(log_path)),
            state: Mutex::new(state),
            listeners: Mutex::default(),
            background: Mutex::default(),
        };
        Self { inner: Arc::new(inner) }
    }

    /// Called whenever servers, their states, or the tools change, until `unsubscribe`.
    pub fn subscribe(&self, listener: impl Fn() + Send + Sync + 'static) -> McpSubscription {
        let id = McpSubscription(NEXT_SUBSCRIPTION.fetch_add(1, Ordering::Relaxed));
        self.inner.listeners.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push((id, Arc::new(listener)));
        id
    }

    /// Stop calling a listener. Returns false when it was not subscribed to this manager.
    pub fn unsubscribe(&self, subscription: McpSubscription) -> bool {
        let mut listeners = self.inner.listeners.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let before = listeners.len();
        listeners.retain(|(id, _)| *id != subscription);
        listeners.len() != before
    }

    pub fn credentials(&self) -> &McpOAuthCredentialStore {
        &self.inner.credentials
    }

    /// Start a session: take the loaded config and connect the enabled servers in the background.
    pub fn start(&self, cwd: PathBuf, loaded: LoadedMcpConfig) {
        let enabled: Vec<u64> = {
            let mut state = self.inner.lock();
            state.session = CancellationToken::new();
            state.cwd = cwd;
            state.config_errors = loaded.errors;
            state.project_config = loaded.project_config;
            state.auto_enable_codemode = loaded.auto_enable_codemode.unwrap_or(true);
            state.warned_unreachable = false;
            state.waited_for_startup = false;
            state.configured = loaded.servers.iter().map(|entry| (entry.name.clone(), entry.source.clone())).collect();
            let (registered, overridden) = self.inner.registered_for_start(&state.configured);
            state.overridden = overridden;
            let configured = loaded.servers.into_iter().map(|entry| (entry, None));
            let all = configured.chain(registered.into_iter().map(|(entry, config)| (entry, Some(config))));
            let mut servers = Vec::new();
            for (entry, registered_config) in all {
                state.next_id += 1;
                servers.push(Server::new(state.next_id, entry, registered_config));
            }
            state.servers = servers;
            state.servers.iter().filter(|server| server.enabled()).map(|server| server.id).collect()
        };
        self.inner.watch_registry();
        self.inner.emit_change();
        if enabled.is_empty() {
            self.inner.report_problems(None);
            return;
        }
        let readies: Vec<Ready> = enabled.iter().map(|id| self.inner.start_connection(*id, None)).collect();
        let (weak, session) = (Arc::downgrade(&self.inner), self.inner.lock().session.clone());
        let task = tokio::spawn(async move {
            futures::future::join_all(readies).await;
            if let Some(inner) = weak.upgrade().filter(|_| !session.is_cancelled()) {
                inner.report_problems(None);
            }
        });
        let pending = async move {
            let _ = task.await;
        };
        self.inner.lock().pending = Some(pending.boxed().shared());
    }

    /// Resolves once the connections started by `start` connected or failed.
    pub async fn pending(&self) {
        let pending = self.inner.lock().pending.clone();
        if let Some(pending) = pending {
            pending.await;
        }
    }

    /// Stop the session: close every connection and wait for running sign-ins and actions.
    pub async fn shutdown(&self) {
        self.inner.unwatch_registry();
        let connections: Vec<McpServerConnection> = {
            let mut state = self.inner.lock();
            state.session.cancel();
            let servers = std::mem::take(&mut state.servers);
            servers.into_iter().filter_map(|server| server.connection).collect()
        };
        self.inner.emit_change();
        let background =
            std::mem::take(&mut *self.inner.background.lock().unwrap_or_else(|poisoned| poisoned.into_inner()));
        let closing = connections.iter().map(|connection| connection.close().boxed());
        futures::future::join(futures::future::join_all(background), futures::future::join_all(closing)).await;
    }

    pub fn connection(&self, name: &str) -> Option<McpServerConnection> {
        self.inner.lock().named(name).and_then(|server| server.connection.clone())
    }

    pub fn entries(&self) -> Vec<McpServerEntry> {
        self.inner.lock().servers.iter().map(|server| server.entry.clone()).collect()
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn emit_change(&self) {
        let listeners = self.listeners.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
        for (_, listener) in listeners {
            listener();
        }
    }

    fn notify(&self, message: &str, level: NotifyLevel) {
        if let Some(notify) = &self.options.notify {
            notify(message, level);
        }
    }

    /// Run work that outlives its caller, so shutdown waits for it.
    fn track<T: Send + 'static>(&self, work: impl Future<Output = T> + Send + 'static) -> tokio::task::JoinHandle<T> {
        let (sender, receiver) = tokio::sync::oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            let _done = sender;
            work.await
        });
        let done: Ready = async move {
            let _ = receiver.await;
        }
        .boxed()
        .shared();
        let mut background = self.background.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        background.retain(|work| work.peek().is_none());
        background.push(done);
        handle
    }

    fn is_current(&self, id: u64, attempt: u64, session: &CancellationToken) -> bool {
        let state = self.lock();
        !session.is_cancelled()
            && state.server(id).is_some_and(|server| server.attempt == Some(attempt) && server.enabled())
    }

    /// Connect the server in the background, after `after`, unless it is disabled, replaced, or
    /// shut down meanwhile. The returned future settles when it connected or failed.
    fn start_connection(self: &Arc<Self>, id: u64, after: Option<Ready>) -> Ready {
        let (attempt, session) = {
            let mut state = self.lock();
            state.next_id += 1;
            let attempt = state.next_id;
            if let Some(server) = state.server_mut(id) {
                server.attempt = Some(attempt);
            }
            (attempt, state.session.clone())
        };
        let weak = Arc::downgrade(self);
        let task = tokio::spawn(async move {
            if let Some(after) = after {
                after.await;
            }
            let Some(inner) = weak.upgrade().filter(|inner| inner.is_current(id, attempt, &session)) else { return };
            let Some(connection) = inner.create_connection(id, attempt, &session) else { return };
            drop(inner);
            let _ = connection.get_client().await;
        });
        let ready: Ready = async move {
            let _ = task.await;
        }
        .boxed()
        .shared();
        if let Some(server) = self.lock().server_mut(id) {
            server.ready = Some(ready.clone());
        }
        ready
    }

    fn create_connection(
        self: &Arc<Self>,
        id: u64,
        attempt: u64,
        session: &CancellationToken,
    ) -> Option<McpServerConnection> {
        let (entry, cwd) = {
            let state = self.lock();
            (state.server(id)?.entry.clone(), state.cwd.clone())
        };
        let mut options = ConnectionOptions::new(entry, cwd, self.credentials.clone());
        options.create_transport = self.options.create_transport.clone();
        options.provider_token = self.options.provider_token.clone();
        options.log = Some(self.log.clone());
        options.client_version = self.options.client_version.clone();
        let (weak, current) = (Arc::downgrade(self), session.clone());
        options.on_tools = Some(Arc::new(move |connection| {
            if let Some(inner) = weak.upgrade().filter(|inner| inner.is_current(id, attempt, &current)) {
                inner.register_tools(connection);
            }
        }));
        let (weak, current) = (Arc::downgrade(self), session.clone());
        options.on_change = Some(Arc::new(move |connection| {
            let Some(inner) = weak.upgrade() else { return };
            if inner.is_current(id, attempt, &current) {
                inner.on_connection_change(connection);
            } else {
                inner.lock().tokens_at_sign_in.retain(|(waiting, _)| !waiting.same(connection));
            }
        }));
        let connection = McpServerConnection::new(options);
        self.lock().server_mut(id)?.connection = Some(connection.clone());
        self.emit_change();
        Some(connection)
    }

    fn stored_tokens(&self, connection: &McpServerConnection) -> String {
        let tokens = connection.oauth_url().and_then(|url| self.credentials.tokens(connection.name(), &url));
        serde_json::to_string(&tokens).unwrap_or_default()
    }

    /// Remember the stored tokens of servers that start needing a sign-in, to notice sign-ins from
    /// other processes (`pi mcp login`).
    fn on_connection_change(&self, connection: &McpServerConnection) {
        let needs_auth = connection.state() == ServerState::NeedsAuth;
        {
            let mut state = self.lock();
            let known = state.tokens_at_sign_in.iter().any(|(waiting, _)| waiting.same(connection));
            if !needs_auth {
                state.tokens_at_sign_in.retain(|(waiting, _)| !waiting.same(connection));
            } else if !known {
                let tokens = self.stored_tokens(connection);
                state.tokens_at_sign_in.push((connection.clone(), tokens));
            }
        }
        self.emit_change();
    }

    fn update_config(&self, entry: &McpServerEntry, patch: &McpServerConfigPatch) -> Result<(), String> {
        if let Some(update) = &self.options.update_config {
            return update(entry, patch);
        }
        let path = PathBuf::from(entry.override_path.as_deref().unwrap_or(&entry.source));
        update_mcp_server_config(&path, &entry.name, *patch, entry.override_path.is_some())
    }
}
