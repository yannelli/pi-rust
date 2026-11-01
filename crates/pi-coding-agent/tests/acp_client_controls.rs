//! Initial ACP discovery, mapped thinking, permission selectors, and cleanup through a real pi session.

#[path = "../../pi-ai/tests/support/fake_acp_agent.rs"]
mod fake_agent;
#[path = "offline_guard/mod.rs"]
mod offline_guard;

use std::sync::Arc;

use pi_ai::auth::InMemoryCredentialStore;
use pi_ai::providers::acp;
use pi_ai::{ContentBlock, Message, ThinkingLevel};
use pi_coding_agent::agent_session::{
    CreateAgentSessionOptions, CreateServicesOptions, ResourceOptions, create_agent_session,
    create_agent_session_services,
};
use pi_coding_agent::session::SessionManager;
use pi_coding_agent::settings::SettingsManager;
use serde_json::json;

const AGENT_ENV: &str = "PI_TEST_FAKE_ACP_CONTROLS";
const TEST_NAME: &str = "fake_acp_agent_process";

#[test]
fn fake_acp_agent_process() {
    if std::env::var_os(AGENT_ENV).is_some() {
        fake_agent::main();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn discovery_and_controls_work_before_the_first_prompt() {
    let root = tempfile::tempdir().unwrap();
    let (cwd, agent_dir) = (root.path().join("work"), root.path().join("agent"));
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&agent_dir).unwrap();
    let settings = Arc::new(SettingsManager::in_memory(json!({
        "defaultProvider": "controls-acp", "defaultModel": "default",
        "acpAgents": [{"id": "controls", "name": "Control test", "command": std::env::current_exe().unwrap(),
            "args": ["--exact", TEST_NAME, "--nocapture", "--quiet", "--test-threads=1"],
            "env": {AGENT_ENV: "1", "FAKE_ACP_CONFIG": "numeric"}}]
    })));
    let services = create_agent_session_services(CreateServicesOptions {
        settings: Some(settings),
        credentials: Some(Arc::new(InMemoryCredentialStore::default())),
        credential_env: Some(offline_guard::credential_free_env()),
        resources: ResourceOptions { home: Some(root.path().to_path_buf()), ..ResourceOptions::default() },
        ..CreateServicesOptions::new(&cwd, &agent_dir)
    })
    .await;
    offline_guard::assert_credentials_hidden(&services);
    let options = CreateAgentSessionOptions::new(services, SessionManager::in_memory(&cwd));
    let session = create_agent_session(options).await.unwrap().session;
    let id = session.session_id();
    let info = acp::open_session("controls-acp", &id).await.unwrap();
    assert!(session.messages().iter().all(|message| !matches!(message, Message::Assistant(_))));
    let models = session.model_runtime().available_models();
    for offered in ["fast", "smart"] {
        assert!(models.iter().any(|model| model.provider == "controls-acp" && model.id == offered));
    }

    let mut foreign = info.clone();
    foreign.pi_session = "another-session".into();
    assert!(!session.refresh_acp_model_metadata(&foreign));
    assert!(session.refresh_acp_model_metadata(&info));
    assert_eq!(session.available_thinking_levels(), [ThinkingLevel::Low, ThinkingLevel::Medium, ThinkingLevel::High]);
    assert_eq!(session.thinking_level(), ThinkingLevel::Medium);

    acp::set_session_option("controls-acp", &id, "permission", "auto").await.unwrap();
    acp::set_session_option("controls-acp", &id, "effortLevel", "3").await.unwrap();
    let info = acp::session_info("controls-acp", &id).unwrap();
    assert!(session.refresh_acp_model_metadata(&info));
    assert_eq!(session.thinking_level(), ThinkingLevel::High);
    assert!(acp::set_session_option("controls-acp", &id, "permission", "unknown").await.is_err());

    session.prompt("hello", Vec::new()).await.unwrap();
    let messages = session.messages();
    let reply = messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::Assistant(reply) => Some(reply),
            _ => None,
        })
        .unwrap();
    assert!(reply.error_message.is_none(), "{:?}", reply.error_message);
    let text: String = reply.content.iter().filter_map(ContentBlock::as_text).collect();
    assert!(text.contains("mode=auto"), "{text}");
    assert!(text.contains("effort=3 news=1"), "{text}");
    assert!(text.contains("turn=1"), "{text}");
    session.shutdown().await;
    assert!(acp::session_info("controls-acp", &id).is_none());
    let error = acp::open_session("controls-acp", &id).await.unwrap_err();
    assert!(error.contains("not a live pi session"), "{error}");
}
