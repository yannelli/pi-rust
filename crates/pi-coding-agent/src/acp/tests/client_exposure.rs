//! `_meta.piAcp.clientMcpExposure`: `"direct"` applies to every client server; other values fail before
//! anything closes or starts; a session reopened without `session/load` gets neither servers nor exposure.

use std::sync::atomic::Ordering;

use agent_client_protocol_schema::{McpServer, Meta};
use pi_mcp::McpExposure;
use serde_json::{Value, json};

use super::{Env, FakeBackend, FakeClient, FakeFactory};
use crate::acp::errors::code;
use crate::acp::mcp_servers::ClientMcpServers;

fn stdio(name: &str) -> Value {
    let command = std::env::temp_dir().join("client-exposure-server").display().to_string();
    json!({"name": name, "command": command, "args": [], "env": []})
}

fn meta(value: Value) -> Option<Meta> {
    value.as_object().cloned()
}

fn exposures(servers: &ClientMcpServers) -> Vec<Option<McpExposure>> {
    servers.entries().iter().map(|entry| entry.config.exposure).collect()
}

fn invalid() -> Vec<Value> {
    vec![
        json!({"piAcp": {"clientMcpExposure": "codemode"}}),
        json!({"piAcp": {"clientMcpExposure": "Direct"}}),
        json!({"piAcp": {"clientMcpExposure": true}}),
        json!({"piAcp": "direct"}),
        json!({"piAcp": null}),
    ]
}

#[test]
fn only_direct_is_accepted_and_it_applies_to_every_client_server() {
    let list: Vec<McpServer> = serde_json::from_value(json!([stdio("a"), stdio("b")])).unwrap();
    for absent in [None, meta(json!({"other": 1})), meta(json!({"piAcp": {}}))] {
        assert_eq!(exposures(&ClientMcpServers::from_request(&list, absent.as_ref()).unwrap()), [None, None]);
    }
    let direct = meta(json!({"piAcp": {"clientMcpExposure": "direct"}}));
    let servers = ClientMcpServers::from_request(&list, direct.as_ref()).unwrap();
    assert_eq!(exposures(&servers), [Some(McpExposure::Direct); 2]);
    for value in invalid() {
        let error = ClientMcpServers::from_request(&list, meta(value.clone()).as_ref()).unwrap_err();
        assert_eq!(code(&error), -32602, "{value}");
    }
}

#[tokio::test]
async fn an_invalid_exposure_starts_nothing_and_keeps_the_live_session() {
    let env = Env::new();
    let (client, backend) = (FakeClient::new(), FakeBackend::new());
    let factory = FakeFactory::returning(backend.clone());
    let agent = env.agent(client, factory.clone());
    let request = serde_json::from_value(json!({"cwd": env.work(), "mcpServers": []})).unwrap();
    let session_id = agent.new_session(request).await.unwrap().response["sessionId"].as_str().unwrap().to_string();
    for value in invalid() {
        let servers = json!([stdio("docs")]);
        let new = json!({"cwd": env.work(), "mcpServers": servers, "_meta": value});
        let load = json!({"sessionId": session_id, "cwd": env.work(), "mcpServers": servers, "_meta": value});
        let errors = [
            agent.new_session(serde_json::from_value(new).unwrap()).await.err().unwrap(),
            agent.load_session(serde_json::from_value(load).unwrap()).await.err().unwrap(),
        ];
        assert!(errors.iter().all(|error| code(error) == -32602), "{value}");
        assert_eq!(factory.spawn_count(), 1, "{value}");
        assert!(agent.session(&session_id).is_some(), "{value}");
        assert_eq!(backend.disposed.load(Ordering::SeqCst), 0, "{value}");
    }
}

#[tokio::test]
async fn the_exposure_is_not_kept_for_a_session_reopened_without_load() {
    let env = Env::new();
    let factory = FakeFactory::returning(FakeBackend::new());
    let agent = env.agent(FakeClient::new(), factory.clone());
    let file = env.work().join("stored.jsonl");
    agent.store.upsert("stored", &env.work().display().to_string(), &file.display().to_string());
    let direct = json!({"piAcp": {"clientMcpExposure": "direct"}});
    let load = json!({"sessionId": "stored", "cwd": env.work(), "mcpServers": [stdio("docs")], "_meta": direct});
    agent.load_session(serde_json::from_value(load).unwrap()).await.unwrap();
    agent.close("stored").await;
    let set_mode = json!({"sessionId": "stored", "modeId": "ask"});
    agent.set_session_mode(serde_json::from_value(set_mode).unwrap()).await.unwrap();
    let recorded = factory.mcp_servers.lock().unwrap().clone();
    let seen: Vec<Vec<Option<McpExposure>>> = recorded.iter().map(exposures).collect();
    assert_eq!(seen, [vec![Some(McpExposure::Direct)], vec![]]);
}
