//! The interactive mode on a virtual terminal over the faux provider. Shared by two test
//! binaries, so each uses only part of it.
#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use pi_ai::auth::InMemoryCredentialStore;
use pi_ai::providers::faux::{FauxProvider, FauxResponse};
use pi_ai::register_api_provider;
use pi_coding_agent::app::AppOptions;
use pi_coding_agent::interactive::{InteractiveMode, InteractiveOptions};
use pi_coding_agent::keybindings::KeybindingsManager;
use pi_coding_agent::session::SessionManager;
use pi_coding_agent::settings::SettingsManager;
use pi_coding_agent::settings::schema::TuiMode;
use pi_tui::testing::VirtualTerminal;
use serde_json::{Value, json};
use tempfile::TempDir;

#[path = "watchdog.rs"]
mod watchdog;
pub use watchdog::Watchdog;

static NEXT_API: AtomicU64 = AtomicU64::new(1);
const TIMEOUT: Duration = Duration::from_secs(10);

pub struct Options {
    pub responses: Vec<FauxResponse>,
    pub settings: Value,
    pub tui_mode: TuiMode,
    pub files: Vec<(&'static str, &'static str)>,
    /// An agent directory to reuse, so several runs share its files.
    pub agent_dir: Option<PathBuf>,
    /// The faux provider waits this long between streamed chunks.
    pub chunk_delay: Duration,
    pub run_limits: pi_coding_agent::agent_session::RunLimits,
    /// The core OAuth flows by provider id in place of pi's.
    pub oauth: Option<pi_ai::auth::OAuthLookup>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            responses: Vec::new(),
            settings: json!({}),
            tui_mode: TuiMode::Regular,
            files: Vec::new(),
            agent_dir: None,
            chunk_delay: Duration::ZERO,
            run_limits: Default::default(),
            oauth: None,
        }
    }
}

pub struct Ui {
    pub _root: TempDir,
    pub agent_dir: PathBuf,
    pub provider: Arc<FauxProvider>,
    pub terminal: Rc<RefCell<VirtualTerminal>>,
    pub mode: Rc<InteractiveMode>,
}

/// Starts the mode on a 100x30 terminal and waits for the footer. Call inside a `LocalSet`.
pub async fn start(options: Options) -> Ui {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("work");
    let agent_dir = options.agent_dir.clone().unwrap_or_else(|| root.path().join("agent"));
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&agent_dir).unwrap();
    for (path, content) in &options.files {
        let path = cwd.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    let api = format!("faux-ui-{}", NEXT_API.fetch_add(1, Ordering::Relaxed));
    let models = json!({"providers": {"local": {"baseUrl": "http://unused", "api": api, "apiKey": "test-key",
        "models": [{"id": "tiny", "contextWindow": 128000, "maxTokens": 4000, "reasoning": true},
            {"id": "other", "contextWindow": 128000, "maxTokens": 4000}]}}});
    std::fs::write(agent_dir.join("models.json"), models.to_string()).unwrap();
    let provider = FauxProvider::with_delay(options.responses, options.chunk_delay);
    register_api_provider(api.clone(), provider.clone());
    let mut values = json!({"defaultProvider": "local", "defaultModel": "tiny", "theme": "dark",
        "enableInstallTelemetry": false, "lastChangelogVersion": "999.0.0"});
    for (key, value) in options.settings.as_object().unwrap() {
        values[key] = value.clone();
    }
    let settings = Arc::new(SettingsManager::in_memory(values));
    let app = AppOptions {
        agent_dir: agent_dir.clone(),
        settings: Some(settings),
        credentials: Some(Arc::new(InMemoryCredentialStore::new(BTreeMap::new()))),
        credential_env: Some(crate::offline_guard::credential_free_env()),
        run_limits: options.run_limits,
        oauth: options.oauth.clone(),
        ..AppOptions::default()
    };
    let manager = SessionManager::create(&cwd, Some(&agent_dir.join("sessions")));
    let runtime = crate::offline_guard::app_runtime(app, manager).await;
    crate::offline_guard::assert_faux_model(runtime.session().model().as_ref(), &api);
    KeybindingsManager::create(&agent_dir).install();
    let terminal = VirtualTerminal::shared(100, 30);
    let interactive = InteractiveOptions {
        terminal: Some(terminal.clone()),
        agent_dir: Some(agent_dir.clone()),
        tui_mode: Some(options.tui_mode),
        skip_update_checks: true,
        ..InteractiveOptions::default()
    };
    let mode = InteractiveMode::new(Arc::new(runtime), interactive);
    let running = mode.clone();
    tokio::task::spawn_local(async move {
        let _ = running.run().await;
    });
    let ui = Ui { _root: root, agent_dir, provider, terminal, mode };
    ui.wait_until("startup", |ui| ui.mode.is_ready()).await;
    ui.wait_for("tiny").await;
    ui
}

impl Ui {
    /// The session's working directory.
    pub fn cwd(&self) -> PathBuf {
        self._root.path().join("work")
    }

    /// The visible rows, joined with newlines.
    pub fn screen(&self) -> String {
        self.terminal.borrow().viewport().join("\n")
    }

    /// Everything written to the terminal so far, with escape sequences.
    pub fn writes(&self) -> String {
        self.terminal.borrow().writes()
    }

    /// Program status reports (OSC 7501) so far.
    pub fn program_statuses(&self) -> Vec<pi_tui::program_status::ProgramStatus> {
        self.terminal.borrow().program_statuses().to_vec()
    }

    /// Lets the UI loop and session tasks run for `millis`.
    pub async fn settle(&self, millis: u64) {
        tokio::time::sleep(Duration::from_millis(millis)).await;
    }

    pub async fn wait_until(&self, what: &str, mut done: impl FnMut(&Ui) -> bool) {
        let started = Instant::now();
        while !done(self) {
            assert!(started.elapsed() < TIMEOUT, "timed out waiting for {what}; screen:\n{}", self.screen());
            self.settle(20).await;
        }
    }

    /// Scrollback and visible rows, joined with newlines.
    pub fn history(&self) -> String {
        self.terminal.borrow_mut().scroll_buffer().join("\n")
    }

    /// Waits until the scrollback or the visible rows contain `text`.
    pub async fn wait_for(&self, text: &str) {
        self.wait_until(text, |ui| ui.history().contains(text)).await;
    }

    /// Sends keys, then lets the UI handle them.
    pub async fn send(&self, keys: &str) {
        self.terminal.borrow().send_input(keys);
        self.settle(60).await;
    }

    /// Types `text` and presses Enter.
    pub async fn submit(&self, text: &str) {
        self.send(text).await;
        self.send("\r").await;
    }

    pub fn session(&self) -> Arc<pi_coding_agent::agent_session::AgentSession> {
        self.mode.current_session()
    }
}

/// Runs `test` on a `LocalSet`, as the interactive mode needs.
pub async fn local<F: Future<Output = ()>>(test: F) {
    tokio::task::LocalSet::new().run_until(test).await;
}
