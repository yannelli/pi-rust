//! Upstream `session-config-options`, `model-thinking-levels`, `session-thinking-modes`, and
//! the agent half of `context-usage`.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use agent_client_protocol_schema::{NewSessionRequest, SetSessionConfigOptionRequest, SetSessionModeRequest};
use serde_json::{Value, json};

use super::{Env, FakeBackend, FakeClient, FakeConfig, FakeFactory, run_after, session_with};
use crate::acp::agent::AcpAgent;
use crate::acp::connection::dispatch;

struct Fixture {
    env: Env,
    client: Arc<FakeClient>,
    backend: Arc<FakeBackend>,
    agent: Arc<AcpAgent>,
}

/// A live session `s1` whose model `reasoning` has levels low, high, max (upstream fixture).
fn fixture() -> Fixture {
    let config = FakeConfig {
        state: json!({"thinkingLevel": "max", "model": {"provider": "test", "id": "reasoning"}}),
        models: Ok(json!({"models": [{"provider": "test", "id": "reasoning"}, {"provider": "test", "id": "plain"}]})),
        levels: Box::new(|state| {
            Ok(if state["model"]["id"] == "plain" {
                vec!["off".into()]
            } else {
                vec!["low".into(), "high".into(), "max".into()]
            })
        }),
        set_thinking: Box::new(|state, _| {
            state["thinkingLevel"] = json!("max");
            Ok(())
        }),
        set_model: Box::new(|state, (provider, id)| {
            state["thinkingLevel"] = json!(if id == "plain" { "off" } else { "max" });
            state["model"] = json!({"provider": provider, "id": id});
            Ok(())
        }),
        ..FakeConfig::default()
    };
    let (env, client, backend) = (Env::new(), FakeClient::new(), FakeBackend::with(config));
    let agent = env.agent(client.clone(), FakeFactory::returning(backend.clone()));
    agent.adopt(session_with(&backend, &client, &env.work()));
    Fixture { env, client, backend, agent }
}

fn option<'a>(options: &'a Value, id: &str) -> &'a Value {
    options.as_array().unwrap().iter().find(|option| option["id"] == id).unwrap()
}

fn values(option: &Value) -> Vec<String> {
    option["options"].as_array().unwrap().iter().map(|choice| choice["value"].as_str().unwrap().to_string()).collect()
}

fn assert_updates(client: &FakeClient, level: &str, levels: &[&str]) {
    let updates = client.updates();
    assert_eq!(updates.len(), 2, "{updates:?}");
    assert_eq!(
        updates[0],
        json!({"sessionId": "s1", "update": {"sessionUpdate": "current_mode_update", "currentModeId": "auto"}})
    );
    let thinking = option(&updates[1]["update"]["configOptions"], "thought_level");
    assert_eq!(thinking["currentValue"], level);
    assert_eq!(values(thinking), levels);
}

async fn set_thinking(f: &Fixture, level: &str) -> Result<Value, agent_client_protocol_schema::Error> {
    let request: SetSessionConfigOptionRequest =
        serde_json::from_value(json!({"sessionId": "s1", "configId": "thought_level", "value": level})).unwrap();
    f.agent.set_session_config_option(request).await.map(|response| serde_json::to_value(response).unwrap())
}

async fn set_model(f: &Fixture, legacy: bool, model: &str) {
    if legacy {
        f.agent.set_session_model("s1", model).await.unwrap();
    } else {
        let request: SetSessionConfigOptionRequest =
            serde_json::from_value(json!({"sessionId": "s1", "configId": "model", "value": model})).unwrap();
        f.agent.set_session_config_option(request).await.unwrap();
    }
}

#[tokio::test]
async fn reasoning_setters_report_the_level_pi_applied() {
    for requested in ["max", "xhigh", "ordinary", " mean ", " "] {
        let f = fixture();
        let result = set_thinking(&f, requested).await.unwrap();
        assert_eq!(option(&result["configOptions"], "thought_level")["currentValue"], "max");
        assert_eq!(f.backend.calls(), [format!("thinking:{requested}")]);
        assert_updates(&f.client, "max", &["low", "high", "max"]);
    }
    let f = fixture();
    f.backend.config().levels = Box::new(|_| Ok(vec!["ordinary".into(), "mean".into()]));
    f.backend.config().set_thinking = Box::new(|state, _| {
        state["thinkingLevel"] = json!("mean");
        Ok(())
    });
    set_thinking(&f, "ordinary").await.unwrap();
    assert_updates(&f.client, "mean", &["ordinary", "mean"]);
}

#[tokio::test]
async fn permission_modes_do_not_change_thinking() {
    for legacy in [false, true] {
        let f = fixture();
        for mode in ["ask", "bypass", "auto"] {
            f.client.clear();
            if legacy {
                let request: SetSessionModeRequest =
                    serde_json::from_value(json!({"sessionId": "s1", "modeId": mode})).unwrap();
                f.agent.set_session_mode(request).await.unwrap();
            } else {
                dispatch(
                    &f.agent,
                    "session/set_config_option",
                    json!({"sessionId": "s1", "configId": "mode", "value": mode}),
                )
                .await
                .unwrap();
            }
            let updates = f.client.updates();
            assert_eq!(updates[0]["update"]["currentModeId"], mode);
            let options = &updates[1]["update"]["configOptions"];
            assert_eq!(option(options, "mode")["currentValue"], mode);
            assert_eq!(option(options, "thought_level")["currentValue"], "max");
        }
        assert_eq!(f.backend.calls(), ["mode:ask", "mode:bypass", "mode:auto"]);
        for mode in ["high", "unknown", ""] {
            let result = dispatch(&f.agent, "session/set_mode", json!({"sessionId": "s1", "modeId": mode})).await;
            assert_eq!(crate::acp::errors::code(&result.err().unwrap()), -32602);
        }
    }
}

#[tokio::test]
async fn model_labels_preserve_ids_and_use_provider_names() {
    let f = fixture();
    f.backend.config().models = Ok(json!({"models": [
        {"provider": "xai", "id": "grok-4.7", "name": "Grok 4.7"},
        {"provider": "xai", "id": "grok-4-fast", "name": "Grok 4 Fast"},
        {"provider": "work", "providerName": "Work Gateway", "id": "custom", "name": "Custom"}
    ]}));
    let config = crate::acp::config::session_configuration(f.backend.as_ref(), None, None).await.unwrap();
    let models = config.models.unwrap().available_models;
    assert_eq!(models[0].model_id, "xai/grok-4.7");
    assert_eq!(models[0].name, "xAI: Grok 4.7");
    assert_eq!(models[1].name, "xAI: Grok 4 Fast ⚡");
    assert_eq!(models[2].name, "Work Gateway: Custom");
}

#[tokio::test]
async fn model_setters_refresh_model_specific_levels_and_usage() {
    for legacy in [false, true] {
        let f = fixture();
        for model in ["plain", "reasoning"] {
            f.client.clear();
            set_model(&f, legacy, &format!("test/{model}")).await;
            let updates: Vec<Value> = f.client.updates().into_iter().take(2).collect();
            let levels = if model == "plain" { vec!["off"] } else { vec!["low", "high", "max"] };
            assert_eq!(updates[0]["update"]["currentModeId"], "auto");
            assert_eq!(values(option(&updates[1]["update"]["configOptions"], "thought_level")), levels);
        }
        assert_eq!(f.backend.calls(), ["model:plain", "model:reasoning"]);
    }
}

#[tokio::test]
async fn failed_reads_after_a_reasoning_change_emit_nothing() {
    for failure in ["state", "discovery", "empty", "inconsistent"] {
        let f = fixture();
        match failure {
            "state" => f.backend.config().state_error = Some("state failed".into()),
            "discovery" => f.backend.config().levels = Box::new(|_| Err(anyhow::anyhow!("discovery failed"))),
            _ => {
                let level = if failure == "empty" { "" } else { "medium" };
                f.backend.config().set_thinking = Box::new(move |state, _| {
                    state["thinkingLevel"] = json!(level);
                    Ok(())
                });
            }
        }
        assert!(set_thinking(&f, "max").await.is_err(), "{failure}");
        assert!(f.client.updates().is_empty(), "{failure}");
    }
}

#[tokio::test]
async fn invalid_requests_do_not_change_pi() {
    let f = fixture();
    for params in [
        json!({"sessionId": "s1", "configId": "thought_level", "value": ""}),
        json!({"sessionId": "s1", "configId": "unknown", "value": "high"}),
        json!({"sessionId": "s1", "configId": "thought_level", "value": 1}),
    ] {
        let error = dispatch(&f.agent, "session/set_config_option", params).await.err().unwrap();
        assert_eq!(crate::acp::errors::code(&error), -32602);
    }
    for mode in [json!(""), json!(1), Value::Null] {
        let error =
            dispatch(&f.agent, "session/set_mode", json!({"sessionId": "s1", "modeId": mode})).await.err().unwrap();
        assert_eq!(crate::acp::errors::code(&error), -32602);
    }
    let error = f.agent.set_session_model("s1", "nope").await.unwrap_err();
    assert_eq!(crate::acp::errors::code(&error), -32602);
    assert!(f.backend.calls().is_empty());
    assert!(f.client.updates().is_empty());
}

#[tokio::test]
async fn model_changes_refresh_usage_but_thinking_changes_do_not() {
    let f = fixture();
    f.backend.config().stats = Box::new(|state| {
        let window = if state["model"]["id"] == "plain" { 200_000 } else { 100_000 };
        Ok(json!({"contextUsage": {"tokens": 500, "contextWindow": window}}))
    });
    set_model(&f, false, "test/plain").await;
    assert_eq!(f.client.kinds(), ["current_mode_update", "config_option_update", "usage_update"]);
    assert_eq!(f.client.update_bodies()[2], json!({"sessionUpdate": "usage_update", "used": 500, "size": 200_000}));
    f.client.clear();
    set_model(&f, true, "reasoning").await;
    assert_eq!(f.client.kinds(), ["current_mode_update", "config_option_update", "usage_update"]);
    let calls = f.backend.stats_calls.load(Ordering::SeqCst);
    f.client.clear();
    set_thinking(&f, "high").await.unwrap();
    assert_eq!(f.backend.stats_calls.load(Ordering::SeqCst), calls);
    assert_eq!(f.client.kinds(), ["current_mode_update", "config_option_update"]);
}

#[tokio::test]
async fn new_sessions_advertise_model_and_thinking_selectors() {
    let config = FakeConfig {
        state: json!({"thinkingLevel": "high", "model": {"provider": "test", "id": "beta"}}),
        models: Ok(json!({"models": [{"provider": "test", "id": "alpha", "name": "Alpha"},
            {"provider": "test", "id": "beta", "name": "Beta"}]})),
        levels: Box::new(|_| {
            Ok(["off", "minimal", "low", "medium", "high", "xhigh", "max"].map(String::from).to_vec())
        }),
        ..FakeConfig::default()
    };
    let (env, client, backend) = (Env::new(), FakeClient::new(), FakeBackend::with(config));
    let agent = env.agent(client.clone(), FakeFactory::returning(backend));
    let request: NewSessionRequest = serde_json::from_value(json!({"cwd": env.work(), "mcpServers": []})).unwrap();
    let result = agent.new_session(request).await.unwrap().response;
    assert_eq!(result["models"]["currentModelId"], "test/beta");
    assert_eq!(result["modes"]["currentModeId"], "auto");
    let levels = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
    let names = ["Off", "Minimal", "Low", "Medium", "High", "Extra High", "Max"];
    let thinking: Vec<Value> =
        levels.iter().zip(names).map(|(level, name)| json!({"value": level, "name": name})).collect();
    let mode = option(&result["configOptions"], "mode");
    assert_eq!(mode["category"], "mode");
    assert_eq!(values(mode), ["auto", "ask", "bypass"]);
    assert_eq!(
        result["configOptions"],
        json!([
            {"type": "select", "id": "model", "category": "model", "name": "Model",
                "description": "Select the model for this session", "currentValue": "test/beta",
                "options": [{"value": "test/alpha", "name": "test: Alpha"}, {"value": "test/beta", "name": "test: Beta"}]},
            {"type": "select", "id": "thought_level", "category": "thought_level", "name": "Thinking",
                "description": "Set the reasoning effort for this session", "currentValue": "high", "options": thinking},
            mode,
        ])
    );
    assert!(client.updates().is_empty(), "updates wait until the response went out");
}

#[tokio::test]
async fn new_session_publishes_usage_and_commands_after_the_response() {
    let mut config = FakeConfig::default();
    config.stats = Box::new(|_| Ok(json!({"contextUsage": {"tokens": 1_234, "contextWindow": 100_000}})));
    config.commands = Ok(json!({"commands": [{"name": "review", "description": "Review", "source": "prompt"},
        {"name": "mcp", "description": "MCP", "source": "extension"}]}));
    let (env, client, backend) = (Env::new(), FakeClient::new(), FakeBackend::with(config));
    std::fs::write(env.agent_dir().join("settings.json"), "{\"quietStartup\": true}").unwrap();
    let agent = env.agent(client.clone(), FakeFactory::returning(backend.clone()));
    let request: NewSessionRequest = serde_json::from_value(json!({"cwd": env.work(), "mcpServers": []})).unwrap();
    let handled = agent.new_session(request).await.unwrap();
    assert!(client.of_kind("usage_update").is_empty());
    let response = run_after(handled).await;
    assert_eq!(response["_meta"]["piAcp"]["startupInfo"], Value::Null, "quietStartup without an update notice");
    assert_eq!(client.kinds(), ["usage_update", "available_commands_update"]);
    assert_eq!(client.update_bodies()[0], json!({"sessionUpdate": "usage_update", "used": 1_234, "size": 100_000}));
    let names: Vec<Value> =
        client.update_bodies()[1]["availableCommands"].as_array().unwrap().iter().map(|c| c["name"].clone()).collect();
    assert_eq!(names[0], "review");
    assert!(!names.contains(&json!("mcp")));
    assert!(names.contains(&json!("compact")) && names.contains(&json!("changelog")));

    backend.config().stats = Box::new(|_| Err(anyhow::anyhow!("unsupported")));
    client.clear();
    let request: NewSessionRequest = serde_json::from_value(json!({"cwd": env.work(), "mcpServers": []})).unwrap();
    run_after(agent.new_session(request).await.unwrap()).await;
    assert_eq!(client.kinds(), ["available_commands_update"]);
}

#[tokio::test]
async fn load_advertises_the_exact_levels_too() {
    for levels in [vec!["off"], vec!["low", "high", "max"], vec!["ordinary", "mean"]] {
        let f = fixture();
        let file = f.env.path().join("thinking.jsonl");
        std::fs::write(&file, "").unwrap();
        f.agent.store.upsert("s2", &f.env.work().display().to_string(), &file.display().to_string());
        let last = levels.last().unwrap().to_string();
        f.backend.config().state["thinkingLevel"] = json!(last);
        let owned: Vec<String> = levels.iter().map(|level| level.to_string()).collect();
        f.backend.config().levels = Box::new(move |_| Ok(owned.clone()));
        let request =
            serde_json::from_value(json!({"sessionId": "s2", "cwd": f.env.work(), "mcpServers": []})).unwrap();
        let result = f.agent.load_session(request).await.unwrap().response;
        assert_eq!(result["modes"]["currentModeId"], "auto");
        assert_eq!(option(&result["configOptions"], "thought_level")["currentValue"], last);
        assert_eq!(values(option(&result["configOptions"], "thought_level")), levels);
        assert_eq!(result["_meta"]["piAcp"]["startupInfo"], Value::Null);
    }
}
