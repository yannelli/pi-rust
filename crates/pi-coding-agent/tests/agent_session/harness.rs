//! A session over the faux provider with recorded events (upstream `test/suite/harness.ts`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_ai::auth::{Credential, InMemoryCredentialStore};
use pi_ai::providers::faux::{FauxProvider, FauxResponse};
use pi_ai::{Context, Message, register_api_provider};
use pi_coding_agent::agent_session::{
    AgentSession, AgentSessionServices, CreateAgentSessionOptions, CreateServicesOptions, ResourceOptions,
    create_agent_session, create_agent_session_services,
};
use pi_coding_agent::app::AppOptions;
use pi_coding_agent::session::SessionManager;
use pi_coding_agent::settings::SettingsManager;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::offline_guard;

static NEXT_API: AtomicU64 = AtomicU64::new(1);

pub struct Options {
    pub settings: Value,
    pub responses: Vec<FauxResponse>,
    pub context_window: u64,
    pub max_tokens: u64,
    pub persist: bool,
    pub delay: Option<Duration>,
    /// Files under the project directory, written before the session starts.
    pub files: Vec<(&'static str, &'static str)>,
    /// More `models.json` providers, by name.
    pub providers: Value,
    /// More fields of the `local/tiny` model, such as `cost` or `promptCache`.
    pub tiny: Value,
    /// Stored credentials by provider.
    pub credentials: BTreeMap<String, Credential>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            settings: json!({}),
            responses: Vec::new(),
            context_window: 128_000,
            max_tokens: 4_000,
            persist: true,
            delay: None,
            files: Vec::new(),
            providers: json!({}),
            tiny: json!({}),
            credentials: BTreeMap::new(),
        }
    }
}

pub struct Harness {
    /// Keeps the temporary directory alive.
    pub _root: TempDir,
    pub provider: Arc<FauxProvider>,
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub settings: Arc<SettingsManager>,
    pub services: AgentSessionServices,
    pub session: Arc<AgentSession>,
    pub events: Arc<Mutex<Vec<Value>>>,
}

fn write_models(agent_dir: &Path, api: &str, options: &Options) {
    let mut models = json!({"providers": {"local": {"baseUrl": "http://unused", "api": api, "apiKey": "test-key",
        "models": [{"id": "tiny", "contextWindow": options.context_window, "maxTokens": options.max_tokens,
            "reasoning": true, "input": ["text", "image"]},
            {"id": "other", "contextWindow": options.context_window, "maxTokens": options.max_tokens}]}}});
    for (key, value) in options.tiny.as_object().unwrap() {
        models["providers"]["local"]["models"][0][key] = value.clone();
    }
    for (name, provider) in options.providers.as_object().unwrap() {
        models["providers"][name] = provider.clone();
    }
    std::fs::write(agent_dir.join("models.json"), models.to_string()).unwrap();
}

/// Record every session event as its JSON form.
pub fn record(session: &AgentSession) -> Arc<Mutex<Vec<Value>>> {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    session.subscribe(move |event| sink.lock().unwrap().push(event.to_json()));
    events
}

pub async fn harness(options: Options) -> Harness {
    harness_with(options, |_| {}).await
}

/// A harness whose session is created with options `configure` adjusts (tools, MCP, ...).
pub async fn harness_with(options: Options, configure: impl FnOnce(&mut CreateAgentSessionOptions)) -> Harness {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("work");
    let agent_dir = root.path().join("agent");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&agent_dir).unwrap();
    for (path, content) in &options.files {
        let path = cwd.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    let api = format!("faux-session-{}", NEXT_API.fetch_add(1, Ordering::Relaxed));
    write_models(&agent_dir, &api, &options);
    let provider = match options.delay {
        Some(delay) => FauxProvider::with_delay(options.responses.clone(), delay),
        None => FauxProvider::new(options.responses.clone()),
    };
    register_api_provider(api.clone(), provider.clone());
    let mut values = json!({"defaultProvider": "local", "defaultModel": "tiny"});
    for (key, value) in options.settings.as_object().unwrap() {
        values[key] = value.clone();
    }
    let settings = Arc::new(SettingsManager::in_memory(values));
    let services = create_agent_session_services(CreateServicesOptions {
        settings: Some(settings.clone()),
        credentials: Some(Arc::new(InMemoryCredentialStore::new(options.credentials.clone()))),
        credential_env: Some(offline_guard::credential_free_env()),
        resources: ResourceOptions { home: Some(root.path().to_path_buf()), ..ResourceOptions::default() },
        ..CreateServicesOptions::new(&cwd, &agent_dir)
    })
    .await;
    let manager = if options.persist {
        SessionManager::create(&cwd, Some(&agent_dir.join("sessions")))
    } else {
        SessionManager::in_memory(&cwd)
    };
    let session = open_guarded(&services, manager, configure, Some(&api)).await;
    let events = record(&session);
    Harness { _root: root, provider, cwd, agent_dir, settings, services, session, events }
}

/// Create a session over `manager` with the given services.
pub async fn open_session(services: &AgentSessionServices, manager: SessionManager) -> Arc<AgentSession> {
    open_session_with(services, manager, |_| {}).await
}

pub async fn open_session_with(
    services: &AgentSessionServices,
    manager: SessionManager,
    configure: impl FnOnce(&mut CreateAgentSessionOptions),
) -> Arc<AgentSession> {
    open_guarded(services, manager, configure, None).await
}

/// Create a session and fail unless its model is offline: the faux model on `faux_api` when
/// given and the test did not choose a model, a faux or loopback model otherwise.
async fn open_guarded(
    services: &AgentSessionServices,
    manager: SessionManager,
    configure: impl FnOnce(&mut CreateAgentSessionOptions),
    faux_api: Option<&str>,
) -> Arc<AgentSession> {
    // Tests run codemode scripts in parallel; pi-codemode tests the worker limit.
    pi_codemode::set_codemode_worker_limit(usize::MAX);
    offline_guard::assert_models_loaded(services);
    offline_guard::assert_credentials_hidden(services);
    let mut options = CreateAgentSessionOptions::new(services.clone(), manager);
    configure(&mut options);
    let chosen = options.model.is_some();
    let session = create_agent_session(options).await.unwrap().session;
    match faux_api.filter(|_| !chosen) {
        Some(api) => offline_guard::assert_faux_model(session.model().as_ref(), api),
        None => offline_guard::assert_offline_model(session.model().as_ref()),
    }
    session
}

impl Harness {
    pub fn events(&self) -> Vec<Value> {
        self.events.lock().unwrap().clone()
    }

    pub fn events_of(&self, kind: &str) -> Vec<Value> {
        self.events().into_iter().filter(|event| event["type"] == kind).collect()
    }

    pub fn requests(&self) -> Vec<Context> {
        self.provider.requests()
    }

    /// App options that build sessions like this harness (for runtime tests).
    pub fn app_options(&self) -> AppOptions {
        AppOptions {
            agent_dir: self.agent_dir.clone(),
            settings: Some(self.settings.clone()),
            credentials: Some(Arc::new(InMemoryCredentialStore::new(BTreeMap::new()))),
            credential_env: Some(offline_guard::credential_free_env()),
            ..AppOptions::default()
        }
    }

    /// The session entries as stored.
    pub fn entries(&self) -> Vec<Value> {
        let manager = self.session.session_manager();
        manager.entries().iter().map(|entry| Value::Object(entry.raw().clone())).collect()
    }
}

/// Event labels as upstream `normalizeEventOrder` builds them.
pub fn event_order(events: &[Value]) -> Vec<String> {
    let mut labels: Vec<String> = Vec::new();
    for event in events {
        let kind = event["type"].as_str().unwrap_or_default();
        let label = match kind {
            "message_start" | "message_end" => format!("{kind}:{}", event["message"]["role"].as_str().unwrap()),
            "tool_execution_start" | "tool_execution_end" => {
                format!("{kind}:{}", event["toolName"].as_str().unwrap())
            }
            _ => kind.to_string(),
        };
        if label == "message_update" && labels.last().is_some_and(|last| last == "message_update") {
            continue;
        }
        labels.push(label);
    }
    labels
}

pub fn roles(messages: &[Message]) -> Vec<&str> {
    messages.iter().map(Message::role).collect()
}

/// Text of a user, assistant, or tool result message.
pub fn text(message: &Message) -> String {
    match message {
        Message::User(user) => user.content.text(),
        Message::Assistant(assistant) => assistant.text(),
        Message::ToolResult(result) => result.content.iter().filter_map(pi_ai::ContentBlock::as_text).collect(),
        Message::System(system) => system.content.text(),
        Message::Custom(custom) => custom.str_field("content").unwrap_or_default().to_string(),
    }
}

/// Poll `condition` every few milliseconds for up to five seconds.
pub async fn wait_until(mut condition: impl FnMut() -> bool) {
    for _ in 0..1000 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("condition not met in time");
}
