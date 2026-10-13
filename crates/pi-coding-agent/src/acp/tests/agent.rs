//! Upstream `new-session-*`, `builtin-commands`, `agent-steering-followup-modes`,
//! `session-restore`, `startup-info-*`, and initialize capability tests.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use agent_client_protocol_schema::{InitializeRequest, NewSessionRequest, PromptRequest, StopReason};
use serde_json::{Value, json};

use super::{Env, FakeBackend, FakeClient, FakeConfig, FakeFactory, session_with, settle};
use crate::acp::agent::AcpAgent;
use crate::acp::errors::code;

fn new_request(cwd: &std::path::Path) -> NewSessionRequest {
    serde_json::from_value(json!({"cwd": cwd, "mcpServers": []})).unwrap()
}

fn prompt_request(session_id: &str, text: &str) -> PromptRequest {
    serde_json::from_value(json!({"sessionId": session_id, "prompt": [{"type": "text", "text": text}]})).unwrap()
}

fn last_text(client: &FakeClient) -> String {
    client.update_bodies().last().unwrap()["content"]["text"].as_str().unwrap().to_string()
}

#[test]
fn initialize_advertises_capabilities_and_terminal_auth() {
    let env = Env::new();
    let agent = env.agent(FakeClient::new(), FakeFactory::returning(FakeBackend::new()));
    let plain: InitializeRequest = serde_json::from_value(json!({"protocolVersion": 1})).unwrap();
    let response = serde_json::to_value(agent.initialize(plain)).unwrap();
    assert_eq!(response["protocolVersion"], 1);
    assert_eq!(response["agentInfo"], json!({"name": "pi", "title": "pi", "version": "1.2.3"}));
    let capabilities = &response["agentCapabilities"];
    assert_eq!(capabilities["loadSession"], true);
    assert_eq!(capabilities["promptCapabilities"], json!({"image": true, "audio": false, "embeddedContext": false}));
    assert_eq!(
        (capabilities["mcpCapabilities"]["http"].clone(), capabilities["mcpCapabilities"]["sse"].clone()),
        (json!(false), json!(false))
    );
    assert_eq!(capabilities["sessionCapabilities"], json!({"list": {}, "delete": {}}));
    assert!(response["authMethods"][0].get("_meta").is_none());
    let zed: InitializeRequest =
        serde_json::from_value(json!({"protocolVersion": 7, "clientCapabilities": {"_meta": {"terminal-auth": true}}}))
            .unwrap();
    let response = serde_json::to_value(agent.initialize(zed)).unwrap();
    assert_eq!(response["protocolVersion"], 1);
    assert_eq!(response["authMethods"][0]["_meta"]["terminal-auth"]["args"], json!(["--terminal-login"]));

    let mut options = env.options();
    options.embedded_context = true;
    let agent = AcpAgent::new(FakeClient::new(), FakeFactory::returning(FakeBackend::new()), options, None);
    let request: InitializeRequest = serde_json::from_value(json!({"protocolVersion": 1})).unwrap();
    let response = serde_json::to_value(agent.initialize(request)).unwrap();
    assert_eq!(response["agentCapabilities"]["promptCapabilities"]["embeddedContext"], true);
}

/// A new-session fixture whose state names a session file under the env.
fn failing_new_session(
    config: FakeConfig,
) -> (Env, Arc<FakeClient>, Arc<FakeBackend>, Arc<AcpAgent>, std::path::PathBuf) {
    let env = Env::new();
    let file = env.path().join("failed.jsonl");
    std::fs::write(&file, "new session\n").unwrap();
    let backend = FakeBackend::with(config);
    backend.config().state["sessionId"] = json!("failed");
    backend.config().state["sessionFile"] = json!(file.display().to_string());
    let client = FakeClient::new();
    let agent = env.agent(client.clone(), FakeFactory::returning(backend.clone()));
    (env, client, backend, agent, file)
}

#[tokio::test]
async fn new_session_requires_auth_without_models_and_cleans_up() {
    let config = FakeConfig { models: Ok(json!({"models": []})), ..FakeConfig::default() };
    let (env, client, backend, agent, file) = failing_new_session(config);
    let error = agent.new_session(new_request(&env.work())).await.err().unwrap();
    assert_eq!(code(&error), -32000);
    assert!(error.message.contains("Configure an API key or log in with an OAuth provider"));
    assert_eq!(error.data.as_ref().unwrap()["authMethods"][0]["id"], "pi_terminal_login");
    assert_eq!(backend.disposed.load(Ordering::SeqCst), 1);
    assert!(!file.exists());
    assert!(agent.store.get("failed").is_none());
    assert!(agent.session("failed").is_none());
    assert!(client.updates().is_empty());
}

#[tokio::test]
async fn model_probe_failures_map_to_auth_or_internal_errors() {
    let config = FakeConfig { models: Err("Authentication required: missing key".into()), ..FakeConfig::default() };
    let (env, _, backend, agent, file) = failing_new_session(config);
    let error = agent.new_session(new_request(&env.work())).await.err().unwrap();
    assert_eq!(code(&error), -32000);
    assert_eq!((backend.disposed.load(Ordering::SeqCst), file.exists()), (1, false));

    let config = FakeConfig { models: Err("socket hang up".into()), ..FakeConfig::default() };
    let (env, _, backend, agent, _) = failing_new_session(config);
    let error = agent.new_session(new_request(&env.work())).await.err().unwrap();
    assert_eq!(code(&error), -32603);
    assert!(error.message.contains("socket hang up"));
    assert_eq!(backend.disposed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn configuration_failures_clean_up_only_the_new_session() {
    for failure in ["discovery", "auth", "invalid-current", "inconsistent-current", "state"] {
        let mut config = FakeConfig::default();
        config.levels = match failure {
            "discovery" => Box::new(|_| Err(anyhow::anyhow!("discovery failed"))),
            "auth" => Box::new(|_| Err(anyhow::anyhow!("Authentication required: missing key"))),
            _ => Box::new(|_| Ok(vec!["low".into(), "high".into(), "max".into()])),
        };
        config.state = json!({"thinkingLevel": match failure { "invalid-current" => "", "inconsistent-current" => "medium", _ => "max" }});
        let (env, client, backend, agent, file) = failing_new_session(config);
        let existing = env.path().join("existing.jsonl");
        std::fs::write(&existing, "existing session\n").unwrap();
        agent.store.upsert("existing", &env.work().display().to_string(), &existing.display().to_string());
        let other = FakeBackend::new();
        agent.adopt(session_with(&other, &client, &env.work()));
        if failure == "state" {
            backend.config().state_error = Some("state read failed".into());
        }
        let error = agent.new_session(new_request(&env.work())).await.err().unwrap();
        assert_eq!(code(&error), if failure == "auth" { -32000 } else { -32603 }, "{failure}");
        assert_eq!(backend.disposed.load(Ordering::SeqCst), 1, "{failure}");
        assert_eq!(other.disposed.load(Ordering::SeqCst), 0, "{failure}");
        assert!(existing.exists() && agent.store.get("existing").is_some(), "{failure}");
        if failure != "state" {
            assert!(!file.exists() && agent.store.get("failed").is_none(), "{failure}");
        }
        assert!(client.updates().is_empty(), "{failure}");
    }
}

#[tokio::test]
async fn new_sessions_need_an_absolute_cwd_and_keep_one_live_session() {
    let env = Env::new();
    let (first, second) = (FakeBackend::new(), FakeBackend::new());
    second.config().state["sessionId"] = json!("second");
    let queue = std::sync::Mutex::new(vec![second.clone(), first.clone()]);
    let factory = FakeFactory::with(Box::new(move |_, _| Ok(queue.lock().unwrap().pop().unwrap())));
    first.config().state["sessionId"] = json!("first");
    let agent = env.agent(FakeClient::new(), factory);
    let error = agent.new_session(new_request(std::path::Path::new("relative/dir"))).await.err().unwrap();
    assert_eq!(code(&error), -32602);
    agent.new_session(new_request(&env.work())).await.unwrap();
    let response = agent.new_session(new_request(&env.work())).await.unwrap().response;
    assert_eq!(response["sessionId"], "second");
    assert_eq!(first.disposed.load(Ordering::SeqCst), 1);
    assert!(agent.session("first").is_none() && agent.session("second").is_some());
}

fn live(env: &Env, config: FakeConfig) -> (Arc<FakeClient>, Arc<FakeBackend>, Arc<AcpAgent>) {
    let (client, backend) = (FakeClient::new(), FakeBackend::with(config));
    let agent = env.agent(client.clone(), FakeFactory::returning(backend.clone()));
    agent.adopt(session_with(&backend, &client, &env.work()));
    (client, backend, agent)
}

#[tokio::test]
async fn steering_and_follow_up_modes_are_answered_by_acp() {
    let env = Env::new();
    let mut config = FakeConfig::default();
    config.state = json!({"thinkingLevel": "medium", "steeringMode": "all", "followUpMode": "one-at-a-time"});
    let (client, backend, agent) = live(&env, config);
    let run = |text: &'static str| {
        let agent = agent.clone();
        async move { agent.prompt(prompt_request("s1", text)).await.unwrap().stop_reason }
    };
    assert_eq!(run("/steering").await, StopReason::EndTurn);
    assert_eq!(last_text(&client), "Steering mode: all");
    run("/steering one-at-a-time").await;
    assert_eq!(last_text(&client), "Steering mode set to: one-at-a-time");
    run("/steering sometimes").await;
    assert_eq!(last_text(&client), "Usage: /steering all | /steering one-at-a-time");
    run("/follow-up").await;
    assert_eq!(last_text(&client), "Follow-up mode: one-at-a-time");
    run("/follow-up ALL").await;
    assert_eq!(last_text(&client), "Follow-up mode set to: all");
    run("/follow-up never").await;
    assert_eq!(last_text(&client), "Usage: /follow-up all | /follow-up one-at-a-time");
    assert_eq!(backend.calls(), ["steering:one-at-a-time", "follow-up:all"]);
    assert!(backend.prompt_texts().is_empty());
}

#[tokio::test]
async fn name_compact_session_and_autocompact_commands() {
    let env = Env::new();
    let mut config = FakeConfig::default();
    config.stats = Box::new(|_| {
        Ok(json!({"sessionId": "s1", "sessionFile": "/tmp/s.jsonl", "totalMessages": 4, "cost": 0.25,
            "tokens": {"input": 10, "output": 5, "cacheRead": 0, "cacheWrite": 0, "total": 15}}))
    });
    let (client, backend, agent) = live(&env, config);
    let run = |text: &'static str| {
        let agent = agent.clone();
        async move { agent.prompt(prompt_request("s1", text)).await.unwrap() }
    };
    run("/name My Session").await;
    assert_eq!(last_text(&client), "Session name set: My Session");
    let info = client.of_kind("session_info_update");
    assert_eq!(info.last().unwrap()["title"], "My Session");
    run("/name").await;
    assert_eq!(last_text(&client), "Usage: /name <name>");
    run("/compact keep the tests").await;
    assert_eq!(
        last_text(&client),
        "Compaction completed. (custom instructions applied)\nTokens before: 1234\n\nShort summary"
    );
    run("/session").await;
    assert_eq!(
        last_text(&client),
        "Session: s1\nSession file: /tmp/s.jsonl\nMessages: 4\nCost: 0.25\nTokens: in 10, out 5, cache read 0, cache write 0, total 15"
    );
    run("/autocompact").await;
    assert_eq!(last_text(&client), "Auto-compaction enabled.");
    run("/autocompact").await;
    assert_eq!(last_text(&client), "Auto-compaction disabled.");
    run("/autocompact on").await;
    assert_eq!(
        backend.calls(),
        ["name:My Session", "compact:keep the tests", "autocompact:true", "autocompact:false", "autocompact:true"]
    );
    run("/export").await;
    assert_eq!(last_text(&client), "Nothing to export yet (no session messages). Send a prompt first.");
    run("/changelog").await;
    assert!(!last_text(&client).is_empty());
    assert!(backend.prompt_texts().is_empty());
}

#[tokio::test]
async fn export_links_the_written_html_file() {
    let env = Env::new();
    let file = env.path().join("s.jsonl");
    std::fs::write(&file, "{\"type\":\"session\"}\n").unwrap();
    let mut config = FakeConfig::default();
    config.state = json!({"thinkingLevel": "medium", "sessionFile": file.display().to_string(), "messageCount": 2});
    let (client, _, agent) = live(&env, config);
    agent.prompt(prompt_request("s1", "/export")).await.unwrap();
    let bodies = client.update_bodies();
    let path = env.work().join("pi-session-s1.html").display().to_string();
    assert_eq!(bodies[bodies.len() - 2]["content"]["text"], "Session exported: ");
    assert_eq!(
        bodies.last().unwrap()["content"],
        json!({"type": "resource_link", "name": "pi-session-s1.html", "uri": format!("file://{path}"),
            "mimeType": "text/html", "title": "Session exported"})
    );
}

#[tokio::test]
async fn prompts_restore_stored_sessions() {
    let env = Env::new();
    let (client, backend) = (FakeClient::new(), FakeBackend::new());
    let factory = FakeFactory::returning(backend.clone());
    let agent = env.agent(client.clone(), factory.clone());
    agent.store.upsert("stored-session", "/tmp/store-project", "/tmp/store-project/session.jsonl");
    let task = {
        let agent = agent.clone();
        tokio::spawn(async move { agent.prompt(prompt_request("stored-session", "hello again")).await })
    };
    settle().await;
    backend.emit(json!({"type": "agent_settled"}));
    assert_eq!(task.await.unwrap().unwrap().stop_reason, StopReason::EndTurn);
    assert_eq!(
        *factory.spawns.lock().unwrap(),
        [("/tmp/store-project".into(), Some("/tmp/store-project/session.jsonl".into()))]
    );
    assert_eq!(backend.prompt_texts(), ["hello again"]);
    assert_eq!(agent.store.get("stored-session").unwrap().cwd, "/tmp/store-project");
    let error = agent.prompt(prompt_request("unknown", "hi")).await.unwrap_err();
    assert_eq!((code(&error), error.data.clone()), (-32602, Some(json!("Unknown sessionId: unknown"))));
}

#[tokio::test]
async fn config_changes_restore_sessions_found_by_discovery() {
    let env = Env::new();
    let dir = env.agent_dir().join("sessions").join("--tmp--fallback-project--");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("0000_restore_fallback.jsonl");
    let header = json!({"type": "session", "version": 3, "id": "fallback-session",
        "timestamp": "2026-06-16T00:00:00.000Z", "cwd": "/tmp/fallback-project"});
    std::fs::write(&file, format!("{header}\n")).unwrap();
    let mut config = FakeConfig::default();
    config.models = Ok(json!({"models": [{"provider": "test", "id": "alpha", "name": "Alpha"},
        {"provider": "test", "id": "beta", "name": "Beta"}]}));
    config.levels = Box::new(|_| Ok(vec!["medium".into()]));
    let (client, backend) = (FakeClient::new(), FakeBackend::with(config));
    let factory = FakeFactory::returning(backend.clone());
    let agent = env.agent(client.clone(), factory.clone());
    let request =
        serde_json::from_value(json!({"sessionId": "fallback-session", "configId": "model", "value": "test/beta"}))
            .unwrap();
    let result = serde_json::to_value(agent.set_session_config_option(request).await.unwrap()).unwrap();
    assert_eq!(*factory.spawns.lock().unwrap(), [("/tmp/fallback-project".into(), Some(file.clone()))]);
    assert_eq!(backend.calls(), ["model:beta"]);
    assert_eq!(result["configOptions"][0]["currentValue"], "test/beta");
    assert_eq!(agent.store.get("fallback-session").unwrap().session_file, file.display().to_string());
    assert_eq!(client.kinds(), ["current_mode_update", "config_option_update"]);
}

#[tokio::test]
async fn cancel_ignores_unknown_sessions() {
    let env = Env::new();
    let (client, factory) = (FakeClient::new(), FakeFactory::returning(FakeBackend::new()));
    let agent = env.agent(client.clone(), factory.clone());
    let cancel = serde_json::from_value(json!({"sessionId": "stale-session"})).unwrap();
    agent.cancel(cancel).await;
    assert_eq!(factory.spawn_count(), 0);
    assert!(client.updates().is_empty());
}

fn startup_info(response: &Value) -> String {
    response["_meta"]["piAcp"]["startupInfo"].as_str().unwrap_or_default().to_string()
}

#[tokio::test]
async fn startup_info_lists_resources_unless_quiet() {
    let env = Env::new();
    let agent_dir = env.agent_dir();
    std::fs::write(agent_dir.join("settings.json"), json!({"packages": ["npm:global-ext"]}).to_string()).unwrap();
    std::fs::create_dir_all(env.work().join(".pi")).unwrap();
    std::fs::write(env.work().join(".pi/settings.json"), json!({"packages": ["/path/to/local-extension"]}).to_string())
        .unwrap();
    std::fs::write(env.work().join("AGENTS.md"), "rules").unwrap();
    std::fs::create_dir_all(agent_dir.join("skills/demo")).unwrap();
    std::fs::write(agent_dir.join("skills/demo/SKILL.md"), "skill").unwrap();
    std::fs::create_dir_all(agent_dir.join("prompts")).unwrap();
    std::fs::write(agent_dir.join("prompts/review.md"), "Review").unwrap();
    let (client, backend) = (FakeClient::new(), FakeBackend::new());
    let agent = env.agent(client.clone(), FakeFactory::returning(backend));
    let handled = agent.new_session(new_request(&env.work())).await.unwrap();
    let info = startup_info(&handled.response);
    assert!(info.starts_with("pi v1.2.3\n---\n"), "{info}");
    let skill = std::path::Path::new("skills").join("demo").join("SKILL.md");
    for expected in [
        "## Context",
        "AGENTS.md",
        "## Skills",
        &skill.display().to_string(),
        "## Prompts",
        "- /review",
        "## Extensions",
        "npm:global-ext\n  - index.ts",
        "/path/to/local-extension",
    ] {
        assert!(info.contains(expected), "{expected} in {info}");
    }
    super::run_after(handled).await;
    assert_eq!(client.update_bodies()[0]["content"]["text"], info.as_str(), "sent once the session exists");

    std::fs::write(env.work().join(".pi/settings.json"), json!({"quietStartup": true}).to_string()).unwrap();
    let handled = agent.new_session(new_request(&env.work())).await.unwrap();
    assert_eq!(handled.response["_meta"]["piAcp"]["startupInfo"], Value::Null);
}

#[test]
fn update_notices_need_a_newer_release() {
    use crate::acp::startup::update_notice;
    use crate::platform::version_check::LatestPiRelease;
    let release = |version: &str| LatestPiRelease { version: version.into(), package_name: None, note: None };
    assert_eq!(
        update_notice(Some(&release("v0.74.0")), "0.73.1").as_deref(),
        Some("New version available: v0.74.0 (installed v0.73.1). Run: `pi update`")
    );
    assert_eq!(update_notice(Some(&release("0.73.1")), "0.73.1"), None);
    assert_eq!(update_notice(None, "0.73.1"), None);
}
