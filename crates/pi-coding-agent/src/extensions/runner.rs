//! The extension runner of one session (upstream `ExtensionRunner`): a JavaScript runtime with the
//! loaded extensions, the registry snapshot they publish, and the session and UI they act on. The
//! runtime and its extensions end with the runner, as upstream reloads extensions per session.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};

use pi_agent::{ToolContext, ToolUpdateCallback};
use pi_ai::EventSender;
use pi_js::{HostFunction, HostValue, JsError, JsHandle, JsRef, JsRuntime, JsRuntimeOptions};
use pi_mcp::McpServerRegistry;
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::registry::Registry;
use super::{modules, ops};
use crate::agent_session::{AgentSession, AgentSessionRuntime, lock};
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

/// One loaded set of extensions. Always held in an `Arc`.
pub struct ExtensionRunner {
    js: JsHandle,
    runtime: Mutex<Option<JsRuntime>>,
    host: OnceLock<JsRef>,
    cwd: PathBuf,
    agent_dir: PathBuf,
    pub(crate) registry: RwLock<Registry>,
    session: RwLock<Weak<AgentSession>>,
    pub(crate) targets: Mutex<HashMap<u64, Weak<AgentSession>>>,
    bindings: RwLock<ExtensionBindings>,
    ids: AtomicU64,
    pub(crate) signals: Mutex<HashMap<u64, CancellationToken>>,
    pub(crate) updates: Mutex<HashMap<u64, ToolUpdateCallback>>,
    pub(crate) sinks: Mutex<HashMap<u64, EventSender>>,
    pub(crate) custom_done: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    pub(crate) data: Mutex<HashMap<u64, DataFn>>,
    pub(crate) nested: Mutex<HashMap<String, ToolContext>>,
    pub(crate) mcp: Arc<McpServerRegistry>,
    load_errors: Mutex<Vec<ExtensionLoadError>>,
    pub(crate) turn_index: AtomicU64,
    pub(crate) project_trusted: AtomicBool,
    pub(crate) terminal_input: AtomicBool,
    stale: AtomicBool,
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
    /// Start a JavaScript runtime with the host modules; no extension is loaded yet.
    pub async fn start(cwd: &Path, agent_dir: &Path) -> anyhow::Result<Arc<Self>> {
        let mut options = JsRuntimeOptions::new(cwd);
        options.name = "pi-extensions".into();
        options.virtual_modules = modules::virtual_modules();
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
                cwd: cwd.to_path_buf(),
                agent_dir: agent_dir.to_path_buf(),
                registry: RwLock::default(),
                session: RwLock::new(Weak::new()),
                targets: Mutex::default(),
                bindings: RwLock::default(),
                ids: AtomicU64::new(1),
                signals: Mutex::default(),
                updates: Mutex::default(),
                sinks: Mutex::default(),
                custom_done: Mutex::default(),
                data: Mutex::default(),
                nested: Mutex::default(),
                mcp: Arc::new(McpServerRegistry::new()),
                load_errors: Mutex::default(),
                turn_index: AtomicU64::new(0),
                project_trusted: AtomicBool::new(true),
                terminal_input: AtomicBool::new(false),
                stale: AtomicBool::new(false),
                weak: weak.clone(),
            }
        });
        let module = runner.js.import(modules::host_specifier()).await.map_err(js_error)?;
        let config = json!({ "cwd": cwd.display().to_string() });
        let host = module.call_method_ref("createHost", vec![config.into()]).await.map_err(js_error)?;
        let _ = runner.host.set(host);
        Ok(runner)
    }

    pub(crate) fn this(&self) -> Arc<Self> {
        self.weak.upgrade().expect("the runner is alive while it is used")
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

    pub fn mcp_registry(&self) -> &Arc<McpServerRegistry> {
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
        let (token, weak) = (token.clone(), self.weak.clone());
        tokio::spawn(async move {
            token.cancelled().await;
            if let Some(runner) = weak.upgrade() {
                let _ = runner.call_json("abortSignal", vec![json!(id)]).await;
            }
        });
        id
    }

    /// Forget a signal id handed to JavaScript once its operation finished.
    pub(crate) fn release_signal(&self, id: u64) {
        let host = self.host().clone();
        tokio::spawn(async move {
            let _ = host.call_method("releaseSignal", vec![json!(id).into()]).await;
        });
    }

    /// The token behind a signal id JavaScript created (`signal.create`).
    pub(crate) fn token(&self, id: Option<u64>) -> CancellationToken {
        id.and_then(|id| lock(&self.signals).get(&id).cloned()).unwrap_or_default()
    }

    /// Stop the JavaScript runtime. Pending calls fail.
    pub fn shutdown(&self) {
        let runtime = lock(&self.runtime).take();
        drop(runtime);
    }
}
