//! The interactive mode on a virtual terminal with one registered ACP agent: this test binary run again as the
//! fake agent of `pi-ai/tests/support/fake_acp_agent.rs` with the `numeric` options. No network or provider.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use pi_ai::Message;
use pi_ai::auth::InMemoryCredentialStore;
use pi_ai::providers::acp;
use pi_ai::providers::faux::FauxProvider;
use pi_coding_agent::app::{AppOptions, create_app_runtime};
use pi_coding_agent::interactive::{InteractiveMode, InteractiveOptions};
use pi_coding_agent::keybindings::KeybindingsManager;
use pi_coding_agent::session::SessionManager;
use pi_coding_agent::settings::SettingsManager;
use pi_coding_agent::settings::schema::TuiMode;
use pi_tui::testing::VirtualTerminal;
use serde_json::{Value, json};

use crate::harness::Ui;

/// Set for the agent process, so the test below runs the fake agent.
pub const AGENT_ENV: &str = "PI_TEST_FAKE_ACP_UI";
pub const PROVIDER: &str = "controls-acp";
const AGENT_TEST: &str = "acp_controls::fake_acp_agent_process";
const DOWN: &str = "\x1b[B";

static SERIAL: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Held for a whole test: the registered agents and the ACP host are process-wide.
pub async fn serialize() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL.lock().await
}

/// Shuts the session down, which stops its agent process, and checks that no agent session remains.
pub async fn shut_down(ui: &Ui) {
    let session = ui.session();
    let id = session.session_id();
    session.shutdown().await;
    assert!(acp::session_info(PROVIDER, &id).is_none(), "the agent session ends with the pi session");
}

/// The `acpAgents` entry; `command` in place of this test binary gives an agent that cannot start.
fn agent_entry(command: Option<&str>) -> Value {
    let exe = std::env::current_exe().unwrap().display().to_string();
    json!({"id": "controls", "name": "Control test", "command": command.map_or(exe, str::to_string),
        "args": ["--exact", AGENT_TEST, "--nocapture", "--quiet", "--test-threads=1"],
        "env": {AGENT_ENV: "1", "FAKE_ACP_CONFIG": "numeric"}})
}

/// Starts the mode with the agent's `default` model selected and waits until the UI is ready. Call
/// inside a `LocalSet`. `provider` of the returned `Ui` is an unused faux provider.
pub async fn start_acp(command: Option<&str>) -> Ui {
    let root = tempfile::tempdir().unwrap();
    let (cwd, agent_dir) = (root.path().join("work"), root.path().join("agent"));
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&agent_dir).unwrap();
    let settings = json!({"defaultProvider": PROVIDER, "defaultModel": acp::DEFAULT_MODEL, "theme": "dark",
        "enableInstallTelemetry": false, "lastChangelogVersion": "999.0.0", "acpAgents": [agent_entry(command)]});
    let mut app = AppOptions {
        agent_dir: agent_dir.clone(),
        settings: Some(Arc::new(SettingsManager::in_memory(settings))),
        credentials: Some(Arc::new(InMemoryCredentialStore::new(BTreeMap::new()))),
        credential_env: Some(crate::offline_guard::credential_free_env()),
        ..AppOptions::default()
    };
    app.resources.home = Some(agent_dir.join("home"));
    let manager = SessionManager::create(&cwd, Some(&agent_dir.join("sessions")));
    let runtime = create_app_runtime(app, manager).await.unwrap();
    crate::offline_guard::assert_models_loaded(&runtime.services());
    crate::offline_guard::assert_credentials_hidden(&runtime.services());
    let model = runtime.session().model().expect("the ACP model");
    assert_eq!((model.api.as_str(), model.provider.as_str()), (acp::API, PROVIDER), "only the local fake agent runs");
    KeybindingsManager::create(&agent_dir).install();
    let terminal = VirtualTerminal::shared(100, 30);
    let interactive = InteractiveOptions {
        terminal: Some(terminal.clone()),
        agent_dir: Some(agent_dir.clone()),
        tui_mode: Some(TuiMode::Regular),
        skip_update_checks: true,
        ..InteractiveOptions::default()
    };
    let mode = InteractiveMode::new(Arc::new(runtime), interactive);
    let running = mode.clone();
    tokio::task::spawn_local(async move {
        let _ = running.run().await;
    });
    let provider = FauxProvider::with_delay(Vec::new(), Duration::ZERO);
    let ui = Ui { _root: root, agent_dir, provider, terminal, mode };
    ui.wait_until("startup", |ui| ui.mode.is_ready()).await;
    ui
}

/// Waits until the agent's models are registered, which only its session reports.
pub async fn wait_discovered(ui: &Ui) {
    ui.wait_until("the agent models", |ui| {
        let models = ui.session().model_runtime().available_models();
        ["fast", "smart"].iter().all(|id| models.iter().any(|model| model.provider == PROVIDER && model.id == *id))
    })
    .await;
}

/// Moves the highlight of an open selector down to `label` and picks it, within eight rows.
pub async fn choose(ui: &Ui, label: &str) {
    let highlighted = format!("→ {label}");
    for _ in 0..8 {
        if ui.screen().contains(&highlighted) {
            return ui.send("\r").await;
        }
        ui.send(DOWN).await;
    }
    panic!("no selector row {label}; screen:\n{}", ui.screen());
}

/// The value the agent reports for its option `id`.
pub fn remote_value(ui: &Ui, id: &str) -> Option<String> {
    let info = acp::session_info(PROVIDER, &ui.session().session_id())?;
    info.selectors.into_iter().find(|selector| selector.id == id)?.current
}

/// Whether the agent answered a prompt in this session.
pub fn has_reply(ui: &Ui) -> bool {
    ui.session().messages().iter().any(|message| matches!(message, Message::Assistant(_)))
}
