//! Sessions with extensions loaded from temporary files, over the faux provider, with a UI that
//! records what extensions ask of it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pi_ai::auth::InMemoryCredentialStore;
use pi_ai::providers::faux::{FauxProvider, FauxResponse};
use pi_ai::register_api_provider;
use pi_coding_agent::agent_session::{
    AgentSession, AgentSessionServices, CreateAgentSessionOptions, CreateServicesOptions, McpSessionOptions,
    create_agent_session, create_agent_session_services,
};
use pi_coding_agent::extension_ui::{DialogOptions, ExtensionMode, ExtensionUi, NotifyType, WidgetPlacement};
use pi_coding_agent::extensions::{
    ExtensionBindings, ExtensionError, ExtensionRunner, InlineExtension, LoadEntry, LoadOptions, load_extensions,
};
use pi_coding_agent::session::SessionManager;
use pi_coding_agent::settings::SettingsManager;
use serde_json::{Value, json};
use tempfile::TempDir;

static NEXT_API: AtomicU64 = AtomicU64::new(1);

/// What the UI was asked, in order, as `kind: detail` lines.
#[derive(Default)]
pub struct RecordingUi {
    pub calls: Mutex<Vec<String>>,
    /// Answers for `select`, `input`, and `editor`, taken in order.
    pub answers: Mutex<Vec<String>>,
    pub confirm: Mutex<bool>,
    pub editor_text: Mutex<String>,
}

impl RecordingUi {
    fn push(&self, line: String) {
        self.calls.lock().unwrap().push(line);
    }

    fn answer(&self) -> Option<String> {
        let mut answers = self.answers.lock().unwrap();
        (!answers.is_empty()).then(|| answers.remove(0))
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl ExtensionUi for RecordingUi {
    async fn select(&self, title: &str, options: &[String], _opts: DialogOptions) -> Option<String> {
        self.push(format!("select: {title} [{}]", options.join(", ")));
        self.answer()
    }

    async fn confirm(&self, title: &str, message: &str, _opts: DialogOptions) -> bool {
        self.push(format!("confirm: {title} / {message}"));
        *self.confirm.lock().unwrap()
    }

    async fn input(&self, title: &str, _placeholder: Option<&str>, _opts: DialogOptions) -> Option<String> {
        self.push(format!("input: {title}"));
        self.answer()
    }

    async fn editor(&self, title: &str, _prefill: Option<&str>, _opts: DialogOptions) -> Option<String> {
        self.push(format!("editor: {title}"));
        self.answer()
    }

    fn fatal_runtime_error(&self, prefix: &str, message: &str, stack: &str) {
        let first = stack.lines().find(|line| line.contains(".ts")).unwrap_or("no stack").trim();
        self.push(format!("fatal: {prefix}: {message} | {first}"));
    }

    fn notify(&self, message: &str, kind: Option<NotifyType>) {
        self.push(format!("notify({}): {message}", kind.unwrap_or_default().as_str()));
    }

    fn set_status(&self, key: &str, text: Option<&str>) {
        self.push(format!("status: {key}={}", text.unwrap_or("<cleared>")));
    }

    fn set_widget(&self, key: &str, lines: Option<&[String]>, placement: Option<WidgetPlacement>) {
        let placement = placement.unwrap_or_default().as_str();
        self.push(format!("widget({placement}): {key}={}", lines.map(|lines| lines.join("|")).unwrap_or_default()));
    }

    fn set_title(&self, title: &str) {
        self.push(format!("title: {title}"));
    }

    fn set_editor_text(&self, text: &str) {
        *self.editor_text.lock().unwrap() = text.to_string();
        self.push(format!("editor_text: {text}"));
    }

    fn get_editor_text(&self) -> String {
        self.editor_text.lock().unwrap().clone()
    }
}

pub struct Setup {
    /// Extension files relative to the extension directory.
    pub files: Vec<(&'static str, String)>,
    /// Files loaded as extensions, relative to the extension directory.
    pub load: Vec<&'static str>,
    pub responses: Vec<FauxResponse>,
    pub settings: Value,
    pub mode: ExtensionMode,
    pub inline: Vec<Arc<dyn InlineExtension>>,
    /// Extension flag values, as from the command line.
    pub flags: Vec<(&'static str, Value)>,
    /// `models.json` in place of the faux provider's.
    pub models: Option<Value>,
    /// List `load` in the `extensions` setting instead of passing the files, as a reload reads it.
    pub from_settings: bool,
    pub bind: bool,
    /// The SDK `session_start_event`.
    pub start_event: Option<Value>,
    /// MCP servers for the session, such as the fake server of `mcp_fake`.
    pub mcp: Option<McpSessionOptions>,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            files: Vec::new(),
            load: Vec::new(),
            responses: Vec::new(),
            settings: json!({}),
            mode: ExtensionMode::Tui,
            inline: Vec::new(),
            flags: Vec::new(),
            models: None,
            from_settings: false,
            bind: true,
            start_event: None,
            mcp: None,
        }
    }
}

#[allow(dead_code)]
pub struct Harness {
    pub _root: TempDir,
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub extension_dir: PathBuf,
    pub provider: Arc<FauxProvider>,
    pub services: AgentSessionServices,
    pub session: Arc<AgentSession>,
    pub runner: Arc<ExtensionRunner>,
    pub ui: Arc<RecordingUi>,
    pub errors: Arc<Mutex<Vec<ExtensionError>>>,
    pub events: Arc<Mutex<Vec<Value>>>,
}

#[allow(dead_code)]
impl Harness {
    pub fn errors(&self) -> Vec<String> {
        self.errors.lock().unwrap().iter().map(|error| format!("{}: {}", error.event, error.error)).collect()
    }

    pub fn events_of(&self, kind: &str) -> Vec<Value> {
        self.events.lock().unwrap().iter().filter(|event| event["type"] == kind).cloned().collect()
    }

    pub fn entries(&self) -> Vec<Value> {
        let manager = self.session.session_manager();
        manager.entries().iter().map(|entry| Value::Object(entry.raw().clone())).collect()
    }

    /// The user text of the last request the model received.
    pub fn last_prompt(&self) -> String {
        let requests = self.provider.requests();
        let Some(context) = requests.last() else { return String::new() };
        let user = context.messages.iter().rev().find(|message| message.role() == "user");
        let value = user.map(|message| serde_json::to_value(message).unwrap()).unwrap_or_default();
        match &value["content"] {
            Value::String(text) => text.clone(),
            Value::Array(blocks) => blocks.iter().filter_map(|block| block["text"].as_str()).collect(),
            _ => String::new(),
        }
    }

    /// Text of every user and assistant message in the transcript.
    pub fn transcript(&self) -> Vec<String> {
        self.session
            .messages()
            .iter()
            .map(|message| {
                let value = serde_json::to_value(message).unwrap();
                let content = &value["content"];
                let text = match content {
                    Value::String(text) => text.clone(),
                    Value::Array(blocks) => blocks.iter().filter_map(|block| block["text"].as_str()).collect(),
                    _ => String::new(),
                };
                format!("{}: {text}", value["role"].as_str().unwrap_or_default())
            })
            .collect()
    }
}

fn write_models(agent_dir: &Path, api: &str) {
    let models = json!({"providers": {"local": {"baseUrl": "http://unused", "api": api, "apiKey": "test-key",
        "models": [{"id": "tiny", "contextWindow": 128000, "maxTokens": 4000, "reasoning": true, "input": ["text", "image"]},
            {"id": "other", "contextWindow": 128000, "maxTokens": 4000}]}}});
    std::fs::write(agent_dir.join("models.json"), models.to_string()).unwrap();
}

pub async fn harness(setup: Setup) -> Harness {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("work");
    let agent_dir = root.path().join("agent");
    let extension_dir = root.path().join("extensions");
    for dir in [&cwd, &agent_dir, &extension_dir] {
        std::fs::create_dir_all(dir).unwrap();
    }
    for (path, content) in &setup.files {
        let path = extension_dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    let api = format!("faux-ext-{}", NEXT_API.fetch_add(1, Ordering::Relaxed));
    write_models(&agent_dir, &api);
    if let Some(models) = &setup.models {
        std::fs::write(agent_dir.join("models.json"), models.to_string()).unwrap();
    }
    let provider = FauxProvider::new(setup.responses.clone());
    register_api_provider(api, provider.clone());
    let mut values = json!({"defaultProvider": "local", "defaultModel": "tiny"});
    if setup.from_settings {
        let paths: Vec<String> = setup.load.iter().map(|path| extension_dir.join(path).display().to_string()).collect();
        values["extensions"] = json!(paths);
    }
    for (key, value) in setup.settings.as_object().unwrap() {
        values[key] = value.clone();
    }
    let settings = Arc::new(SettingsManager::in_memory(values));
    let services = create_agent_session_services(CreateServicesOptions {
        settings: Some(settings),
        credentials: Some(Arc::new(InMemoryCredentialStore::new(Default::default()))),
        ..CreateServicesOptions::new(&cwd, &agent_dir)
    })
    .await;
    let entries = setup.load.iter().map(|path| LoadEntry::file(extension_dir.join(path))).collect();
    let flag_values = setup.flags.iter().map(|(name, value)| (name.to_string(), value.clone())).collect();
    let entries = (!setup.from_settings).then_some(entries);
    let options = LoadOptions { entries, inline: setup.inline.clone(), flag_values };
    let loaded = load_extensions(&services, &options).await;
    let runner = loaded.runner.expect("extensions loaded");
    let errors = Arc::new(Mutex::new(Vec::new()));
    for error in runner.load_errors() {
        errors.lock().unwrap().push(ExtensionError {
            extension_path: error.path,
            event: "load".into(),
            error: error.error,
            stack: None,
        });
    }
    let manager = SessionManager::create(&cwd, Some(&agent_dir.join("sessions")));
    let mut create = CreateAgentSessionOptions::new(services.clone(), manager);
    create.extensions = Some(runner.clone());
    create.session_start_event = setup.start_event.clone();
    if let Some(mcp) = &setup.mcp {
        create.mcp = mcp.clone();
    }
    let created = create_agent_session(create).await.unwrap();
    assert!(created.extensions.is_some_and(|attached| Arc::ptr_eq(&attached, &runner)));
    let session = created.session;
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    session.subscribe(move |event| sink.lock().unwrap().push(event.to_json()));
    let ui = Arc::new(RecordingUi::default());
    if setup.bind {
        let sink = errors.clone();
        let bindings = ExtensionBindings {
            ui: ui.clone(),
            mode: setup.mode,
            on_error: Some(Arc::new(move |error: &ExtensionError| sink.lock().unwrap().push(error.clone()))),
            ..ExtensionBindings::default()
        };
        session.bind_extensions(bindings).await;
    }
    Harness { _root: root, cwd, agent_dir, extension_dir, provider, services, session, runner, ui, errors, events }
}

/// An upstream example extension from the fixtures.
pub fn example(path: &str) -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/extensions");
    std::fs::read_to_string(root.join(path)).unwrap()
}

/// Wait until `condition` holds, for work extensions start without awaiting it.
pub async fn eventually(condition: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !condition() {
        assert!(std::time::Instant::now() < deadline, "condition not reached in time");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}
