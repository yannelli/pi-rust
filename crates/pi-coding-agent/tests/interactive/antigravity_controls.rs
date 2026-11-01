//! Built-in Antigravity controls on the virtual terminal before the first prompt. The test runs itself again with
//! `PI_ANTIGRAVITY_ACP_SERVER` set to a script that starts this binary once more as the fake ACP server.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use pi_ai::auth::{Credential, InMemoryCredentialStore, OAuthCredential};
use pi_ai::providers::antigravity_acp;
use pi_ai::providers::faux::FauxProvider;
use pi_ai::{Message, ThinkingLevel};
use pi_coding_agent::app::{AppOptions, create_app_runtime};
use pi_coding_agent::interactive::{InteractiveMode, InteractiveOptions};
use pi_coding_agent::keybindings::KeybindingsManager;
use pi_coding_agent::session::SessionManager;
use pi_coding_agent::settings::SettingsManager;
use pi_coding_agent::settings::schema::TuiMode;
use pi_tui::testing::VirtualTerminal;
use serde_json::{Value, json};

use crate::harness::{Ui, local};

/// Set for the fake server process, with the request log path as its value.
const AGENT_ENV: &str = "PI_TEST_FAKE_ANTIGRAVITY_LOG";
/// Set for the run of the UI test in a child process.
const UI_ENV: &str = "PI_TEST_ANTIGRAVITY_UI";
const AGENT_TEST: &str = "antigravity_controls::fake_antigravity_server_process";
const UI_TEST: &str = "antigravity_controls::mode_and_thinking_are_confirmed_before_the_first_prompt";
const DOWN: &str = "\x1b[B";

/// The fake server: JSON-RPC on stdio, each request logged with this process ID.
#[test]
fn fake_antigravity_server_process() {
    let Some(log) = std::env::var_os(AGENT_ENV) else { return };
    let mut log = std::fs::OpenOptions::new().create(true).append(true).open(log).unwrap();
    let [mut model, mut thought, mut approval, mut mode] = ["gemini-3.8-flash", "low", "ask", "plan"].map(String::from);
    let config = |model: &str, thought: &str, approval: &str| {
        let options =
            |values: &[&str]| -> Vec<Value> { values.iter().map(|v| json!({"value": v, "name": v})).collect() };
        json!([
            {"id": "model", "category": "model", "type": "select", "currentValue": model,
             "options": options(&["gemini-3.8-flash", "gemini-3.1-pro"])},
            {"id": "thought", "category": "thought_level", "type": "select", "currentValue": thought,
             "options": options(&["low", "high"])},
            {"id": "approval", "name": "Approval", "category": "permission", "type": "select",
             "currentValue": approval, "options": options(&["ask", "auto"])},
        ])
    };
    let mut out = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        let Ok(message) = serde_json::from_str::<Value>(&line.unwrap()) else { continue };
        let (Some(method), id, params) = (message["method"].as_str(), &message["id"], &message["params"]) else {
            continue;
        };
        writeln!(log, "{}", json!({"pid": std::process::id(), "method": method, "params": params})).unwrap();
        let result = match method {
            "initialize" => json!({"protocolVersion": 1, "agentCapabilities": {
                "promptCapabilities": {"embeddedContext": true, "image": true},
                "_meta": {"antigravity": {"systemPrompt": {"append": true}}}}}),
            "session/new" => json!({"sessionId": format!("s{}", std::process::id()),
                "configOptions": config(&model, &thought, &approval),
                "modes": {"currentModeId": mode, "availableModes": [{"id": "plan", "name": "Plan"}, {"id": "code", "name": "Code"}]}}),
            "session/set_config_option" => {
                let value = params["value"].as_str().unwrap_or_default().to_string();
                match params["configId"].as_str() {
                    Some("model") => model = value,
                    Some("thought") => thought = value,
                    _ => approval = value,
                }
                json!({"configOptions": config(&model, &thought, &approval)})
            }
            "session/set_mode" => {
                mode = params["modeId"].as_str().unwrap_or_default().to_string();
                json!({})
            }
            "session/prompt" => {
                let chunk =
                    json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "done"}});
                let update = json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": params["sessionId"], "update": chunk}});
                writeln!(out, "{update}").unwrap();
                json!({"stopReason": "end_turn"})
            }
            _ if id.is_null() => continue,
            _ => json!({}),
        };
        if !id.is_null() {
            writeln!(out, "{}", json!({"jsonrpc": "2.0", "id": id, "result": result})).unwrap();
            out.flush().unwrap();
        }
    }
}

/// Runs the UI test in a child with the fake server installed, then checks the order of what the server got.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mode_and_thinking_are_confirmed_before_the_first_prompt() {
    if std::env::var_os(UI_ENV).is_some() {
        return local(run_ui()).await;
    }
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let (script, log) = (dir.path().join("agy_acp_server"), dir.path().join("requests.jsonl"));
    let exe = std::env::current_exe().unwrap();
    let body =
        format!("#!/bin/sh\nexec '{}' --exact {AGENT_TEST} --nocapture --quiet --test-threads=1\n", exe.display());
    std::fs::write(&script, body).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut child = tokio::process::Command::new(&exe);
    child.kill_on_drop(true);
    child.args(["--exact", UI_TEST, "--nocapture", "--test-threads=1"]);
    child.env(UI_ENV, "1").env("PI_ANTIGRAVITY_ACP_SERVER", &script).env(AGENT_ENV, &log).env("PI_OFFLINE", "1");
    let output =
        tokio::time::timeout(Duration::from_secs(240), child.output()).await.expect("the UI run ends").unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success() && text.contains("1 passed"), "the UI run failed:\n{text}");
    assert_wire_order(&log);
}

/// One server got the bridged session; it saw the mode and the thinking level confirmed before the prompt.
fn assert_wire_order(log: &Path) {
    let lines = std::fs::read_to_string(log).unwrap();
    let requests: Vec<Value> = lines.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    let bridged = |request: &&Value| {
        request["method"] == "session/new" && request["params"]["mcpServers"].to_string().contains("pi-bridge")
    };
    let live: Vec<&Value> = requests.iter().filter(bridged).collect();
    assert_eq!(live.len(), 1, "one live session, reused by the prompt: {requests:#?}");
    let pid = &live[0]["pid"];
    let steps: Vec<String> = requests
        .iter()
        .filter(|request| &request["pid"] == pid)
        .map(|request| match request["method"].as_str().unwrap() {
            "session/set_config_option" => {
                format!("set {}={}", request["params"]["configId"], request["params"]["value"])
            }
            other => other.to_string(),
        })
        .collect();
    let at = |step: &str| steps.iter().position(|seen| seen == step).unwrap_or_else(|| panic!("no {step}: {steps:?}"));
    let order = [at("session/new"), at(r#"set "approval"="auto""#), at(r#"set "thought"="low""#), at("session/prompt")];
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "out of order: {steps:?}");
    assert_eq!(steps.iter().filter(|step| *step == "session/new").count(), 1, "{steps:?}");
    assert!(!steps.iter().any(|step| step.contains("claude")), "a model the agent lacks is never sent: {steps:?}");
}

async fn run_ui() {
    let ui = start().await;
    let id = ui.session().session_id();
    ui.wait_until("the live Antigravity session", |_| antigravity_acp::session_info(&id).is_some()).await;
    assert_eq!(ui.session().thinking_level(), ThinkingLevel::High, "pi shows the agent's confirmed level");
    assert!(!ui.session().messages().iter().any(|message| matches!(message, Message::Assistant(_))));
    ui.submit("/acp mode").await;
    ui.wait_for("Approval").await;
    choose(&ui, "Approval").await;
    ui.wait_for("auto").await;
    choose(&ui, "auto").await;
    ui.wait_for("Approval: auto").await;
    ui.submit("/thinking low").await;
    ui.send("\r").await;
    ui.wait_for("Thinking level: low").await;
    assert_eq!(ui.session().thinking_level(), ThinkingLevel::Low, "pi keeps the level the agent confirmed");
    let info = antigravity_acp::session_info(&id).unwrap();
    let current = |id: &str| info.selectors.iter().find(|selector| selector.id == id).and_then(|s| s.current.clone());
    assert_eq!((current("approval").as_deref(), current("thought").as_deref()), (Some("auto"), Some("low")));
    let rejected = "Antigravity does not offer model claude-opus-5-5";
    ui.submit("/model google-antigravity/claude-opus-5-5").await;
    ui.send("\r").await;
    ui.wait_for(rejected).await;
    assert_eq!(ui.session().model().unwrap().id, "gemini-3.1-pro", "pi keeps its model when the agent rejects one");
    ui.send("\x10").await;
    ui.wait_until("the rejected cycle", |ui| ui.history().matches(rejected).count() >= 2).await;
    assert_eq!(ui.session().model().unwrap().id, "gemini-3.1-pro", "Ctrl+P switches only to a confirmed model");
    ui.submit("hi").await;
    ui.wait_until("the agent reply", |ui| {
        ui.session().messages().iter().any(|message| matches!(message, Message::Assistant(_)))
    })
    .await;
    ui.session().shutdown().await;
    assert!(antigravity_acp::session_info(&id).is_none(), "the session ends with the pi session");
}

async fn choose(ui: &Ui, label: &str) {
    let highlighted = format!("→ {label}");
    for _ in 0..8 {
        if ui.screen().contains(&highlighted) {
            return ui.send("\r").await;
        }
        ui.send(DOWN).await;
    }
    panic!("no selector row {label}; screen:\n{}", ui.screen());
}

async fn start() -> Ui {
    let root = tempfile::tempdir().unwrap();
    let (cwd, agent_dir) = (root.path().join("work"), root.path().join("agent"));
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&agent_dir).unwrap();
    let settings = json!({"defaultProvider": "google-antigravity", "defaultModel": "gemini-3.1-pro",
        "defaultThinkingLevel": "high", "theme": "dark",
        "enabledModels": ["google-antigravity/gemini-3.1-pro", "google-antigravity/claude-opus-5-5"], "enableInstallTelemetry": false,
        "lastChangelogVersion": "999.0.0"});
    let marker = Credential::OAuth(OAuthCredential {
        refresh: "antigravity-acp".into(),
        access: "antigravity-acp".into(),
        expires: i64::MAX,
        ..Default::default()
    });
    let credentials = BTreeMap::from([("google-antigravity".to_string(), marker)]);
    let mut app = AppOptions {
        agent_dir: agent_dir.clone(),
        settings: Some(Arc::new(SettingsManager::in_memory(settings))),
        credentials: Some(Arc::new(InMemoryCredentialStore::new(credentials))),
        credential_env: Some(crate::offline_guard::credential_free_env()),
        ..AppOptions::default()
    };
    app.resources.home = Some(agent_dir.join("home"));
    let manager = SessionManager::create(&cwd, Some(&agent_dir.join("sessions")));
    let runtime = create_app_runtime(app, manager).await.unwrap();
    let model = runtime.session().model().expect("the Antigravity model");
    assert_eq!(model.api, antigravity_acp::API, "only the local fake server runs");
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
