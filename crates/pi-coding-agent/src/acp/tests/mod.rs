//! Unit tests of ACP mode, ported from pi-acp's `test/unit` and `test/component` with fakes
//! in place of the pi process and the ACP connection (upstream `test/helpers/fakes.ts`).
#![allow(clippy::field_reassign_with_default)]

mod agent;
mod config;
mod events;
mod listing;
mod permissions;
mod session;
mod translate;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use agent_client_protocol_schema::{
    Error, RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome,
    SessionNotification,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use super::agent::{AcpAgent, AcpAgentOptions};
use super::auth::TerminalLaunch;
use super::backend::{AcpClient, BackendFactory, PiBackend};
use super::session::{AcpSession, AcpSessionParams};

type Hook<T> = Box<dyn Fn(&mut Value, T) -> anyhow::Result<()> + Send + Sync>;
type Read<T> = Box<dyn Fn(&Value) -> anyhow::Result<T> + Send + Sync>;

/// The client side: records every notification and answers permission requests.
pub struct FakeClient {
    pub updates: Mutex<Vec<SessionNotification>>,
    pub permission_requests: Mutex<Vec<RequestPermissionRequest>>,
    pub next_permission: Mutex<Result<RequestPermissionResponse, Error>>,
}

impl FakeClient {
    pub fn new() -> Arc<Self> {
        let selected = RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new("allow"));
        Arc::new(Self {
            updates: Mutex::new(Vec::new()),
            permission_requests: Mutex::new(Vec::new()),
            next_permission: Mutex::new(Ok(RequestPermissionResponse::new(selected))),
        })
    }

    /// Notifications as JSON `{sessionId, update}`.
    pub fn updates(&self) -> Vec<Value> {
        self.updates.lock().unwrap().iter().map(|update| serde_json::to_value(update).unwrap()).collect()
    }

    /// Only the `update` objects.
    pub fn update_bodies(&self) -> Vec<Value> {
        self.updates().into_iter().map(|update| update["update"].clone()).collect()
    }

    pub fn kinds(&self) -> Vec<String> {
        self.update_bodies().iter().map(|update| update["sessionUpdate"].as_str().unwrap().to_string()).collect()
    }

    pub fn of_kind(&self, kind: &str) -> Vec<Value> {
        self.update_bodies().into_iter().filter(|update| update["sessionUpdate"] == kind).collect()
    }

    pub fn clear(&self) {
        self.updates.lock().unwrap().clear();
    }

    pub fn answer(&self, response: Result<RequestPermissionResponse, Error>) {
        *self.next_permission.lock().unwrap() = response;
    }
}

#[async_trait]
impl AcpClient for FakeClient {
    fn session_update(&self, notification: SessionNotification) {
        self.updates.lock().unwrap().push(notification);
    }

    async fn request_permission(&self, request: RequestPermissionRequest) -> Result<RequestPermissionResponse, Error> {
        self.permission_requests.lock().unwrap().push(request);
        self.next_permission.lock().unwrap().clone()
    }
}

/// What a [`FakeBackend`] answers; tests change fields to script pi.
pub struct FakeConfig {
    pub state: Value,
    pub state_error: Option<String>,
    pub models: Result<Value, String>,
    pub levels: Read<Vec<String>>,
    pub stats: Read<Value>,
    pub stats_delay: Option<Duration>,
    pub messages: Value,
    pub commands: Result<Value, String>,
    pub prompt_error: Option<String>,
    pub set_thinking: Hook<String>,
    pub set_model: Hook<(String, String)>,
    pub set_name: Hook<String>,
}

impl Default for FakeConfig {
    fn default() -> Self {
        Self {
            state: json!({"thinkingLevel": "medium", "model": {"provider": "test", "id": "model"}}),
            state_error: None,
            models: Ok(json!({"models": [{"provider": "test", "id": "model", "name": "model"}]})),
            levels: Box::new(|_| Ok(vec!["medium".into(), "high".into()])),
            stats: Box::new(|_| Ok(json!({}))),
            stats_delay: None,
            messages: json!({"messages": []}),
            commands: Ok(json!({"commands": []})),
            prompt_error: None,
            set_thinking: Box::new(|state, level| {
                state["thinkingLevel"] = json!(level);
                Ok(())
            }),
            set_model: Box::new(|state, (provider, id)| {
                state["model"] = json!({"provider": provider, "id": id});
                Ok(())
            }),
            set_name: Box::new(|state, name| {
                state["sessionName"] = json!(name);
                Ok(())
            }),
        }
    }
}

/// A scripted pi session (upstream `FakePiRpcProcess`).
pub struct FakeBackend {
    pub config: Mutex<FakeConfig>,
    pub prompts: Mutex<Vec<(String, Vec<pi_ai::ContentBlock>)>>,
    pub ui_responses: Mutex<Vec<Value>>,
    pub calls: Mutex<Vec<String>>,
    pub aborts: AtomicUsize,
    pub stats_calls: AtomicUsize,
    pub disposed: AtomicUsize,
    sender: UnboundedSender<Value>,
    events: Mutex<Option<UnboundedReceiver<Value>>>,
}

impl FakeBackend {
    pub fn new() -> Arc<Self> {
        Self::with(FakeConfig::default())
    }

    pub fn with(config: FakeConfig) -> Arc<Self> {
        let (sender, receiver) = unbounded_channel();
        Arc::new(Self {
            config: Mutex::new(config),
            prompts: Mutex::new(Vec::new()),
            ui_responses: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
            aborts: AtomicUsize::new(0),
            stats_calls: AtomicUsize::new(0),
            disposed: AtomicUsize::new(0),
            sender,
            events: Mutex::new(Some(receiver)),
        })
    }

    pub fn config(&self) -> MutexGuard<'_, FakeConfig> {
        self.config.lock().unwrap()
    }

    /// Send an event through the session's event pump.
    pub fn emit(&self, event: Value) {
        self.sender.send(event).unwrap();
    }

    pub fn prompt_texts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().iter().map(|(text, _)| text.clone()).collect()
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn call(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }
}

fn fail(message: &Option<String>) -> anyhow::Result<()> {
    match message {
        Some(message) => Err(anyhow::anyhow!(message.clone())),
        None => Ok(()),
    }
}

#[async_trait]
impl PiBackend for FakeBackend {
    fn bind_client(&self, _: &str, _: Arc<dyn AcpClient>) {}

    fn permission_mode(&self) -> crate::acp::permissions::PermissionMode {
        crate::acp::permissions::PermissionMode::parse(self.config().state["permissionMode"].as_str().unwrap_or("auto"))
            .unwrap()
    }

    fn set_permission_mode(&self, mode: crate::acp::permissions::PermissionMode) {
        self.call(format!("mode:{}", mode.id()));
        self.config().state["permissionMode"] = json!(mode.id());
    }

    fn take_events(&self) -> Option<UnboundedReceiver<Value>> {
        self.events.lock().unwrap().take()
    }

    async fn prompt(&self, message: String, images: Vec<pi_ai::ContentBlock>) -> anyhow::Result<()> {
        self.prompts.lock().unwrap().push((message, images));
        fail(&self.config().prompt_error)
    }

    async fn abort(&self) -> anyhow::Result<()> {
        self.aborts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn get_state(&self) -> anyhow::Result<Value> {
        let config = self.config();
        fail(&config.state_error)?;
        Ok(config.state.clone())
    }

    async fn get_available_models(&self) -> anyhow::Result<Value> {
        self.config().models.clone().map_err(|message| anyhow::anyhow!(message))
    }

    async fn set_model(&self, provider: &str, model_id: &str) -> anyhow::Result<()> {
        self.call(format!("model:{model_id}"));
        let mut config = self.config();
        let config = &mut *config;
        (config.set_model)(&mut config.state, (provider.to_string(), model_id.to_string()))
    }

    async fn get_available_thinking_levels(&self) -> anyhow::Result<Vec<String>> {
        let config = self.config();
        (config.levels)(&config.state)
    }

    async fn set_thinking_level(&self, level: &str) -> anyhow::Result<()> {
        self.call(format!("thinking:{level}"));
        let mut config = self.config();
        let config = &mut *config;
        (config.set_thinking)(&mut config.state, level.to_string())
    }

    async fn set_steering_mode(&self, mode: &str) -> anyhow::Result<()> {
        self.call(format!("steering:{mode}"));
        self.config().state["steeringMode"] = json!(mode);
        Ok(())
    }

    async fn set_follow_up_mode(&self, mode: &str) -> anyhow::Result<()> {
        self.call(format!("follow-up:{mode}"));
        self.config().state["followUpMode"] = json!(mode);
        Ok(())
    }

    async fn compact(&self, custom_instructions: Option<String>) -> anyhow::Result<Value> {
        self.call(format!("compact:{}", custom_instructions.unwrap_or_default()));
        Ok(json!({"summary": "Short summary", "tokensBefore": 1234}))
    }

    async fn set_auto_compaction(&self, enabled: bool) -> anyhow::Result<()> {
        self.call(format!("autocompact:{enabled}"));
        self.config().state["autoCompactionEnabled"] = json!(enabled);
        Ok(())
    }

    async fn get_session_stats(&self) -> anyhow::Result<Value> {
        self.stats_calls.fetch_add(1, Ordering::SeqCst);
        let delay = self.config().stats_delay;
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        let config = self.config();
        (config.stats)(&config.state)
    }

    async fn set_session_name(&self, name: &str) -> anyhow::Result<()> {
        self.call(format!("name:{name}"));
        let mut config = self.config();
        let config = &mut *config;
        (config.set_name)(&mut config.state, name.to_string())
    }

    async fn export_html(&self, output_path: &Path) -> anyhow::Result<PathBuf> {
        Ok(output_path.to_path_buf())
    }

    async fn get_messages(&self) -> anyhow::Result<Value> {
        Ok(self.config().messages.clone())
    }

    async fn get_commands(&self) -> anyhow::Result<Value> {
        self.config().commands.clone().map_err(|message| anyhow::anyhow!(message))
    }

    async fn send_extension_ui_response(&self, response: Value) -> anyhow::Result<()> {
        self.ui_responses.lock().unwrap().push(response);
        Ok(())
    }

    async fn dispose(&self) {
        self.disposed.fetch_add(1, Ordering::SeqCst);
    }
}

type Spawn = Box<dyn Fn(&Path, Option<&Path>) -> Result<Arc<FakeBackend>, Error> + Send + Sync>;

/// Opens scripted backends and records each spawn.
pub struct FakeFactory {
    pub spawns: Mutex<Vec<(PathBuf, Option<PathBuf>)>>,
    pub spawn: Mutex<Spawn>,
}

impl FakeFactory {
    /// Every spawn returns `backend`.
    pub fn returning(backend: Arc<FakeBackend>) -> Arc<Self> {
        Self::with(Box::new(move |_, _| Ok(backend.clone())))
    }

    pub fn with(spawn: Spawn) -> Arc<Self> {
        Arc::new(Self { spawns: Mutex::new(Vec::new()), spawn: Mutex::new(spawn) })
    }

    pub fn spawn_count(&self) -> usize {
        self.spawns.lock().unwrap().len()
    }
}

#[async_trait]
impl BackendFactory for FakeFactory {
    async fn create(&self, cwd: &Path, session_file: Option<&Path>) -> Result<Arc<dyn PiBackend>, Error> {
        self.spawns.lock().unwrap().push((cwd.to_path_buf(), session_file.map(Path::to_path_buf)));
        let backend = (self.spawn.lock().unwrap())(cwd, session_file)?;
        Ok(backend)
    }
}

/// A temporary pi home: agent dir, sessions, and the session map.
pub struct Env {
    pub root: tempfile::TempDir,
}

impl Env {
    pub fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("agent/sessions")).unwrap();
        std::fs::create_dir_all(root.path().join("work")).unwrap();
        Self { root }
    }

    pub fn path(&self) -> &Path {
        self.root.path()
    }

    pub fn work(&self) -> PathBuf {
        self.path().join("work")
    }

    pub fn agent_dir(&self) -> PathBuf {
        self.path().join("agent")
    }

    pub fn options(&self) -> AcpAgentOptions {
        AcpAgentOptions {
            agent_dir: self.agent_dir(),
            home: self.path().to_path_buf(),
            sessions_root: self.agent_dir().join("sessions"),
            session_map_path: self.path().join("pi-acp/session-map.json"),
            embedded_context: false,
            terminal_launch: TerminalLaunch { command: "pi".into(), args: vec!["--terminal-login".into()] },
            version: "1.2.3".into(),
        }
    }

    pub fn agent(&self, client: Arc<FakeClient>, factory: Arc<FakeFactory>) -> Arc<AcpAgent> {
        AcpAgent::new(client, factory, self.options(), None)
    }
}

/// A session over `backend` reporting to `client`, as upstream tests build `PiAcpSession`.
pub fn session_with(backend: &Arc<FakeBackend>, client: &Arc<FakeClient>, cwd: &Path) -> Arc<AcpSession> {
    AcpSession::new(AcpSessionParams {
        session_id: "s1".into(),
        cwd: cwd.to_path_buf(),
        backend: backend.clone(),
        client: client.clone(),
        auto_title: false,
    })
}

/// Let spawned tasks run.
pub async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(5)).await;
}

/// Run the work a response defers.
pub async fn run_after<T>(handled: super::agent::Handled<T>) -> T {
    if let Some(after) = handled.after {
        after.await;
    }
    handled.response
}
