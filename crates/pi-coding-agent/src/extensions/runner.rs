//! The extension runner of one session (upstream `ExtensionRunner`): a JavaScript runtime with the
//! loaded extensions, the registry snapshot they publish, and the session and UI they act on. The
//! runtime and its extensions end with the runner, as upstream reloads extensions per session.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};

use futures::future::BoxFuture;
use pi_agent::{ToolContext, ToolUpdateCallback};
use pi_ai::{ApiProvider, EventSender};
use pi_js::{HostFunction, HostValue, JsBackend, JsError, JsHandle, JsRef, JsRuntime, JsRuntimeOptions};
use pi_mcp::McpServerRegistry;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::backend::ExtensionRuntime;
use super::inline::InlineExtension;
use super::provider::LoginRequest;
use super::registry::Registry;
use super::{modules, ops};
use crate::agent_session::{AgentSession, AgentSessionRuntime, ModelRuntime, lock};
use crate::extension_ui::{ExtensionMode, ExtensionUi, NoOpExtensionUi};

/// A failed extension handler (upstream `ExtensionError`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionError {
    pub extension_path: String,
    pub event: String,
    pub error: String,
    pub stack: Option<String>,
}

/// An extension that could not load.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionLoadError {
    pub path: String,
    pub error: String,
}

pub type ExtensionErrorFn = Arc<dyn Fn(&ExtensionError) + Send + Sync>;
pub type ShutdownFn = Arc<dyn Fn() + Send + Sync>;

/// What the running mode supplies (upstream `ExtensionBindings`).
#[derive(Clone)]
pub struct ExtensionBindings {
    pub ui: Arc<dyn ExtensionUi>,
    pub mode: ExtensionMode,
    /// Runs `ctx.newSession()`, `fork()`, `switchSession()`, and `reload()`; without it they do
    /// nothing and report not cancelled, as upstream's defaults.
    pub runtime: Option<Weak<AgentSessionRuntime>>,
    pub shutdown: Option<ShutdownFn>,
    /// `ctx.abort()`; the session's abort when unset.
    pub abort: Option<ShutdownFn>,
    pub on_error: Option<ExtensionErrorFn>,
}

impl Default for ExtensionBindings {
    fn default() -> Self {
        Self {
            ui: Arc::new(NoOpExtensionUi),
            mode: ExtensionMode::Print,
            runtime: None,
            shutdown: None,
            abort: None,
            on_error: None,
        }
    }
}

pub(crate) type DataFn = Arc<dyn Fn(&str) + Send + Sync>;

/// An API implementation a runner replaced, and the provider that owns the replacement.
pub(crate) type ReplacedApi = (Option<Arc<dyn ApiProvider>>, Option<String>);

/// One loaded set of extensions. Always held in an `Arc`.
pub struct ExtensionRunner {
    js: JsHandle,
    runtime: Mutex<Option<JsRuntime>>,
    host: OnceLock<JsRef>,
    backend: JsBackend,
    /// The `--extension-runtime` choice, passed on to the runner a reload starts.
    requested_runtime: OnceLock<ExtensionRuntime>,
    /// The mode the session binds later; `config.info` reports it while extensions load.
    load_mode: OnceLock<crate::extension_ui::ExtensionMode>,
    cwd: PathBuf,
    agent_dir: PathBuf,
    pub(crate) registry: RwLock<Registry>,
    session: RwLock<Weak<AgentSession>>,
    pub(crate) targets: Mutex<HashMap<u64, Weak<AgentSession>>>,
    pub(crate) sdk: super::ops_sdk::SdkState,
    pub(crate) resources: Mutex<super::ops_sdk_resources::ResourcesState>,
    /// Set for the child host of an SDK `DefaultResourceLoader`.
    pub(crate) parent: OnceLock<super::ops_sdk_resources::ParentLink>,
    pub(crate) model_runtimes: super::ops_sdk_model_runtime::ModelRuntimes,
    bindings: RwLock<ExtensionBindings>,
    ids: AtomicU64,
    pub(crate) signals: Mutex<HashMap<u64, CancellationToken>>,
    pub(crate) updates: Mutex<HashMap<u64, ToolUpdateCallback>>,
    pub(crate) sinks: Mutex<HashMap<u64, EventSender>>,
    pub(crate) custom_done: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    pub(crate) data: Mutex<HashMap<u64, DataFn>>,
    pub(crate) nested: Mutex<HashMap<String, ToolContext>>,
    pub(crate) mcp: McpServerRegistry,
    model_runtime: Arc<ModelRuntime>,
    /// Providers this runner registered, with the extension that registered each.
    pub(crate) providers: Mutex<Vec<(String, String)>>,
    /// APIs this runner replaced: the replaced implementation and the owning provider.
    pub(crate) apis: Mutex<HashMap<String, ReplacedApi>>,
    pub(crate) logins: Mutex<HashMap<u64, mpsc::UnboundedSender<LoginRequest>>>,
    pub(crate) virtual_models: Mutex<Vec<(String, String)>>,
    pub(crate) inline: Mutex<Vec<Arc<dyn InlineExtension>>>,
    /// Extension paths that registered tools with the session.
    pub(crate) tool_paths: Mutex<std::collections::BTreeSet<String>>,
    /// Watchers of signal ids handed to JavaScript, stopped on release.
    watchers: Mutex<HashMap<u64, CancellationToken>>,
    load_errors: Mutex<Vec<ExtensionLoadError>>,
    pub(crate) terminal_input: AtomicBool,
    stale: AtomicBool,
    /// Queue of `emit_detached` events, delivered one at a time in order.
    pub(crate) detached: OnceLock<mpsc::UnboundedSender<Value>>,
    weak: Weak<ExtensionRunner>,
}

impl std::fmt::Debug for ExtensionRunner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("ExtensionRunner").field("cwd", &self.cwd).finish_non_exhaustive()
    }
}

fn js_error(error: JsError) -> anyhow::Error {
    anyhow::anyhow!(error.display_with_stack())
}

impl ExtensionRunner {
    /// Start a JavaScript runtime on `backend` with the host modules; no extension is loaded yet.
    pub fn start(
        cwd: &Path,
        agent_dir: &Path,
        model_runtime: Arc<ModelRuntime>,
        backend: JsBackend,
    ) -> BoxFuture<'static, anyhow::Result<Arc<Self>>> {
        let (cwd, agent_dir) = (cwd.to_path_buf(), agent_dir.to_path_buf());
        Box::pin(async move { Self::start_at(cwd, agent_dir, model_runtime, backend).await })
    }

    async fn start_at(
        cwd: PathBuf,
        agent_dir: PathBuf,
        model_runtime: Arc<ModelRuntime>,
        backend: JsBackend,
    ) -> anyhow::Result<Arc<Self>> {
        let mut options = JsRuntimeOptions::new(&cwd);
        options.backend = backend.clone();
        options.name = "pi-extensions".into();
        options.virtual_modules = modules::virtual_modules();
        options.env = Some(pi_ai::env::process_env());
        if matches!(backend, JsBackend::Node { .. }) {
            options.cache_dir = Some(agent_dir.join("cache").join("pi-js-node"));
        }
        // Host work runs on the caller's runtime, so operations survive the runner they started from.
        options.tokio = tokio::runtime::Handle::try_current().ok();
        let runtime = JsRuntime::new(options).map_err(js_error)?;
        runtime.register_source_files(modules::source_files());
        let js = (*runtime).clone();
        let runner = Arc::new_cyclic(|weak: &Weak<ExtensionRunner>| {
            let sync_weak = weak.clone();
            js.register_host_function(
                "ext.sync",
                HostFunction::sync(move |args| match sync_weak.upgrade() {
                    Some(runner) => ops::dispatch_sync(&runner, args),
                    None => Err(JsError::new("The extension host has shut down")),
                }),
            );
            let async_weak = weak.clone();
            js.register_host_function(
                "ext.async",
                HostFunction::asynchronous(move |args| {
                    let runner = async_weak.upgrade();
                    async move {
                        match runner {
                            Some(runner) => ops::dispatch_async(runner, args).await,
                            None => Err(JsError::new("The extension host has shut down")),
                        }
                    }
                }),
            );
            ExtensionRunner {
                js: js.clone(),
                runtime: Mutex::new(Some(runtime)),
                host: OnceLock::new(),
                backend,
                requested_runtime: OnceLock::new(),
                load_mode: OnceLock::new(),
                cwd: cwd.clone(),
                agent_dir,
                registry: RwLock::default(),
                session: RwLock::new(Weak::new()),
                targets: Mutex::default(),
                sdk: Default::default(),
                resources: Mutex::default(),
                parent: OnceLock::new(),
                model_runtimes: Default::default(),
                bindings: RwLock::default(),
                ids: AtomicU64::new(1),
                signals: Mutex::default(),
                updates: Mutex::default(),
                sinks: Mutex::default(),
                custom_done: Mutex::default(),
                data: Mutex::default(),
                nested: Mutex::default(),
                mcp: McpServerRegistry::new(),
                model_runtime,
                providers: Mutex::default(),
                apis: Mutex::default(),
                logins: Mutex::default(),
                virtual_models: Mutex::default(),
                inline: Mutex::default(),
                tool_paths: Mutex::default(),
                watchers: Mutex::default(),
                load_errors: Mutex::default(),
                terminal_input: AtomicBool::new(false),
                stale: AtomicBool::new(false),
                detached: OnceLock::new(),
                weak: weak.clone(),
            }
        });
        let module = runner.js.import(modules::host_specifier()).await.map_err(js_error)?;
        let config = json!({ "cwd": cwd.display().to_string() });
        let host = module.call_method_ref("createHost", vec![config.into()]).await.map_err(js_error)?;
        let _ = runner.host.set(host);
        runner.watch_mcp_servers();
        Ok(runner)
    }

    pub(crate) fn weak(&self) -> Weak<Self> {
        self.weak.clone()
    }

    /// The model runtime providers and virtual models register with.
    pub fn model_runtime(&self) -> &Arc<ModelRuntime> {
        &self.model_runtime
    }

    pub(crate) fn this(&self) -> Arc<Self> {
        self.weak.upgrade().expect("the runner is alive while it is used")
    }

    /// The engine running this runner's extensions, after any fallback to QuickJS.
    pub fn backend(&self) -> &JsBackend {
        &self.backend
    }

    pub(crate) fn requested_runtime(&self) -> Option<ExtensionRuntime> {
        self.requested_runtime.get().copied()
    }

    pub(crate) fn load_mode(&self) -> Option<crate::extension_ui::ExtensionMode> {
        self.load_mode.get().copied()
    }

    pub(crate) fn set_load_mode(&self, mode: crate::extension_ui::ExtensionMode) {
        let _ = self.load_mode.set(mode);
    }

    pub(crate) fn set_requested_runtime(&self, runtime: ExtensionRuntime) {
        let _ = self.requested_runtime.set(runtime);
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn agent_dir(&self) -> &Path {
        &self.agent_dir
    }

    pub(crate) fn js(&self) -> &JsHandle {
        &self.js
    }

    pub(crate) fn host(&self) -> &JsRef {
        self.host.get().expect("the host object is created in start()")
    }

    pub(crate) fn next_id(&self) -> u64 {
        self.ids.fetch_add(1, Ordering::Relaxed)
    }

    /// Call a method of the JavaScript host object.
    pub(crate) async fn call(&self, method: &str, args: Vec<HostValue>) -> Result<HostValue, JsError> {
        self.host().call_method(method, args).await
    }

    pub(crate) async fn call_json(&self, method: &str, args: Vec<Value>) -> Result<Value, JsError> {
        let args = args.into_iter().map(HostValue::Json).collect();
        Ok(self.call(method, args).await?.into_json())
    }

    /// Load extension files in order. Failures are kept as [`Self::load_errors`].
    pub async fn load_entries(&self, entries: &[super::loader::LoadEntry]) {
        let entries: Vec<Value> = entries.iter().map(super::loader::LoadEntry::to_json).collect();
        let errors = match self.call_json("load", vec![Value::Array(entries)]).await {
            Ok(Value::Array(errors)) => errors
                .iter()
                .map(|error| ExtensionLoadError {
                    path: error["path"].as_str().unwrap_or_default().to_string(),
                    error: error["error"].as_str().unwrap_or_default().to_string(),
                })
                .collect(),
            Ok(_) => Vec::new(),
            Err(error) => vec![ExtensionLoadError { path: "<runtime>".into(), error: error.display_with_stack() }],
        };
        lock(&self.load_errors).extend(errors);
    }

    pub fn load_errors(&self) -> Vec<ExtensionLoadError> {
        lock(&self.load_errors).clone()
    }

    /// Paths of the loaded extensions, in load order.
    pub fn extension_paths(&self) -> Vec<String> {
        self.registry().extensions.iter().map(|extension| extension.path.clone()).collect()
    }

    pub fn registry(&self) -> std::sync::RwLockReadGuard<'_, Registry> {
        self.registry.read().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn has_handlers(&self, event: &str) -> bool {
        self.registry().handlers.contains(event)
    }

    /// Servers extensions registered; the session's built-in MCP connects them.
    pub fn mcp_registry(&self) -> &McpServerRegistry {
        &self.mcp
    }

    /// The session this runner acts on, once bound.
    pub fn session(&self) -> Option<Arc<AgentSession>> {
        self.session.read().unwrap_or_else(|poisoned| poisoned.into_inner()).upgrade()
    }

    pub(crate) fn set_session(&self, session: &Arc<AgentSession>) {
        *self.session.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::downgrade(session);
    }

    pub fn bindings(&self) -> ExtensionBindings {
        self.bindings.read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    pub(crate) fn set_bindings(&self, bindings: ExtensionBindings) {
        *self.bindings.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = bindings;
    }

    pub fn mode(&self) -> ExtensionMode {
        self.bindings().mode
    }

    /// Report a failed handler to the bound error listener, or stderr without one.
    pub fn emit_error(&self, error: ExtensionError) {
        match self.bindings().on_error {
            Some(listener) => listener(&error),
            None => eprintln!("Extension error ({}, {}): {}", error.extension_path, error.event, error.error),
        }
    }

    /// Mark captured `pi` and `ctx` objects stale (after session replacement or reload).
    pub async fn invalidate(&self, message: Option<&str>) {
        if self.stale.swap(true, Ordering::SeqCst) {
            return;
        }
        let message = message.map(Value::from).unwrap_or(Value::Null);
        let _ = self.call_json("invalidate", vec![message]).await;
    }

    pub fn is_stale(&self) -> bool {
        self.stale.load(Ordering::SeqCst)
    }

    /// A token id JavaScript sees as an `AbortSignal`, aborted when `token` is cancelled.
    pub(crate) fn signal_for(&self, token: &CancellationToken) -> u64 {
        let id = self.next_id();
        let released = CancellationToken::new();
        lock(&self.watchers).insert(id, released.clone());
        let (token, weak) = (token.clone(), self.weak.clone());
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                tokio::select! {
                    () = token.cancelled() => {}
                    () = released.cancelled() => return,
                }
                if let Some(runner) = weak.upgrade() {
                    let _ = runner.call_json("abortSignal", vec![json!(id)]).await;
                }
            });
        }
        id
    }

    /// A signal id for an event field (`signal` of `session_before_compact`), aborted with `token`.
    pub fn signal_id(&self, token: &CancellationToken) -> u64 {
        self.signal_for(token)
    }

    pub fn release_signal_id(&self, id: u64) {
        self.release_signal(id);
    }

    /// Forget a signal id handed to JavaScript once its operation finished.
    pub(crate) fn release_signal(&self, id: u64) {
        if let Some(watcher) = lock(&self.watchers).remove(&id) {
            watcher.cancel();
        }
        let host = self.host().clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = host.call_method("releaseSignal", vec![json!(id).into()]).await;
            });
        }
    }

    /// The token behind a signal id JavaScript created (`signal.create`).
    pub(crate) fn token(&self, id: Option<u64>) -> CancellationToken {
        id.and_then(|id| lock(&self.signals).get(&id).cloned()).unwrap_or_default()
    }

    /// Stop the JavaScript runtime and restore the APIs it replaced. Pending calls fail.
    pub fn shutdown(&self) {
        self.sdk.shutdown();
        super::ops_sdk_resources::shutdown(self);
        self.restore_apis();
        for (_, watcher) in lock(&self.watchers).drain() {
            watcher.cancel();
        }
        let runtime = lock(&self.runtime).take();
        // Dropping the runtime joins its thread, which must not be this one.
        if let Some(runtime) = runtime {
            std::thread::spawn(move || drop(runtime));
        }
    }
}
