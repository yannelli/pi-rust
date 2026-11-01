use std::sync::Arc;

use pi_ai::auth::InMemoryCredentialStore;
use pi_ai::providers::acp::{self, AcpHost, AgentCommand, Choice, PermissionRequest, Selector, SessionInfo};
use pi_ai::types::ThinkingLevel;
use serde_json::json;

use super::{host, register};
use crate::agent_session::{ModelRuntime, ModelRuntimeOptions};
use crate::settings::SettingsManager;

static GLOBAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn runtime(dir: &std::path::Path) -> Arc<ModelRuntime> {
    let options = ModelRuntimeOptions {
        models_path: dir.join("models.json"),
        credentials: Arc::new(InMemoryCredentialStore::default()),
        oauth: None,
        credential_env: Arc::new(|_: &str| None::<String>),
    };
    Arc::new(ModelRuntime::create(options).await)
}

#[tokio::test]
async fn enabled_agents_become_providers_with_advertised_models() {
    let _guard = GLOBAL.lock().await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("acp")).unwrap();
    let state =
        json!({"models": {"kiro-acp": [{"id": "claude-x", "name": "Claude X"}]}, "thinking": {"kiro-acp": true}});
    std::fs::write(host::state_path(dir.path()), state.to_string()).unwrap();
    let settings = SettingsManager::in_memory(json!({"acpAgents": [
        {"id": "kiro", "name": "Kiro", "command": "kiro-cli", "args": ["acp"]},
        {"id": "off", "name": "Off", "command": "off", "enabled": false}]}));
    let runtime = runtime(dir.path()).await;
    register::sync(&runtime, &settings, dir.path());

    let default = runtime.get_model("kiro-acp", acp::DEFAULT_MODEL).unwrap();
    assert_eq!((default.api.as_str(), default.reasoning), (acp::API, true));
    assert!(runtime.get_model("kiro-acp", "claude-x").is_some());
    assert!(runtime.available_models().iter().any(|model| model.provider == "kiro-acp"));
    assert!(runtime.get_model("off-acp", acp::DEFAULT_MODEL).is_none());
    assert_eq!(acp::agent("kiro-acp").unwrap().args, ["acp"]);

    let installed = host::install(dir.path(), &runtime);
    let choice = Choice { id: "claude-y".into(), name: "Claude Y".into(), description: None };
    let selector =
        Selector { id: "model".into(), category: "model".into(), choices: vec![choice], ..Selector::default() };
    let info = SessionInfo {
        provider: "kiro-acp".into(),
        pi_session: "p1".into(),
        acp_session: "s9".into(),
        selectors: vec![selector],
        ..SessionInfo::default()
    };
    installed.session_changed(&info);
    assert!(runtime.get_model("kiro-acp", "claude-y").is_some());
    assert!(runtime.get_model("kiro-acp", "claude-x").is_none());
    assert_eq!(installed.recall_session("kiro-acp", "p1").as_deref(), Some("s9"));

    let web = AgentCommand { name: "web".into(), description: "Search".into(), hint: None };
    let unknown = SessionInfo { commands_known: false, commands: Vec::new(), ..info.clone() };
    installed.session_changed(&SessionInfo { commands_known: true, commands: vec![web], ..info.clone() });
    installed.session_changed(&unknown);
    assert_eq!(host::commands(dir.path(), "kiro-acp").len(), 1, "a session that sent no update keeps them");
    installed.session_changed(&SessionInfo { commands_known: true, ..unknown });
    assert!(host::commands(dir.path(), "kiro-acp").is_empty(), "an empty update clears the editor commands");

    let request = PermissionRequest {
        provider: "kiro-acp".into(),
        agent_name: "Kiro".into(),
        pi_session: "p1".into(),
        title: "Edit".into(),
        kind: None,
        options: vec![
            ("y".into(), "Allow".into(), "allow_once".into()),
            ("n".into(), "Reject".into(), "reject_once".into()),
        ],
        tool_call: json!({}),
    };
    assert_eq!(installed.request_permission(request).await.as_deref(), Some("n"));
}

#[tokio::test]
async fn advertised_thought_levels_become_the_thinking_level_map() {
    let _guard = GLOBAL.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let settings =
        SettingsManager::in_memory(json!({"acpAgents": [{"id": "kiro", "name": "Kiro", "command": "kiro"}]}));
    let runtime = runtime(dir.path()).await;
    register::sync(&runtime, &settings, dir.path());
    let installed = host::install(dir.path(), &runtime);
    let choice = |id: &str, name: &str| Choice { id: id.into(), name: name.into(), description: None };
    let choices = vec![choice("1", "Low"), choice("3", "HIGH"), choice("auto", "Auto")];
    let effort =
        Selector { id: "effortLevel".into(), category: "thought_level".into(), choices, ..Selector::default() };
    let info = SessionInfo {
        provider: "kiro-acp".into(),
        pi_session: "p1".into(),
        acp_session: "s1".into(),
        selectors: vec![effort],
        ..SessionInfo::default()
    };
    installed.session_changed(&info);
    let model = runtime.get_model("kiro-acp", acp::DEFAULT_MODEL).unwrap();
    let map = model.thinking_level_map.clone().expect("a map from the advertised levels");
    assert_eq!((map["low"].as_deref(), map["high"].as_deref()), (Some("1"), Some("3")));
    assert_eq!((map["off"].as_deref(), map["medium"].as_deref()), (None, None));
    assert_eq!(pi_ai::simple_options::supported_thinking_levels(&model), [ThinkingLevel::Low, ThinkingLevel::High]);
    assert_eq!(host::read_state(dir.path()).thought_levels["kiro-acp"].len(), 3);

    installed.session_changed(&SessionInfo { selectors: Vec::new(), ..info });
    let model = runtime.get_model("kiro-acp", acp::DEFAULT_MODEL).unwrap();
    assert!(!model.reasoning && model.thinking_level_map.is_none());
    assert!(register::thinking_level_map(&[choice("auto", "Auto")]).is_none(), "no pi level, no map");
}
