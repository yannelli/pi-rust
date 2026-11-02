//! MCP servers from the ACP client: checked before anything starts, passed to the factory exactly as
//! sent, and never kept for a session that is reopened without `session/load`.

use std::sync::atomic::Ordering;

use agent_client_protocol_schema::McpServer;
use pi_mcp::McpConfigScope;
use serde_json::{Value, json};

use super::{Env, FakeBackend, FakeClient, FakeConfig, FakeFactory};
use crate::acp::errors::code;
use crate::acp::mcp_servers::ClientMcpServers;

const SECRET: &str = "SENTINEL_7f3a";

fn command() -> String {
    std::env::temp_dir().join("client-mcp-server").display().to_string()
}

fn stdio(name: &str) -> Value {
    json!({"name": name, "command": command(), "args": ["~/x", "$HOME", "!echo hi", "${A}", SECRET],
        "env": [{"name": "TOKEN", "value": format!("!cat {SECRET} $HOME ~/u")}]})
}

fn parsed(servers: &Value) -> Vec<McpServer> {
    serde_json::from_value(servers.clone()).unwrap()
}

fn assert_literal(servers: &ClientMcpServers) {
    let [entry] = servers.entries() else { panic!("one server expected: {servers:?}") };
    assert_eq!(
        (entry.name.as_str(), entry.source.as_str(), entry.scope),
        ("docs", "ACP client", Some(McpConfigScope::Client))
    );
    assert_eq!(entry.config.command.as_deref(), Some(command().as_str()));
    let args: Vec<String> = ["~/x", "$HOME", "!echo hi", "${A}", SECRET].map(String::from).into();
    assert_eq!(entry.config.args.as_ref(), Some(&args));
    let env = json!({"TOKEN": format!("!cat {SECRET} $HOME ~/u")});
    assert_eq!(entry.config.env.as_ref().map(|env| Value::Object(env.clone())), Some(env));
}

#[tokio::test]
async fn new_and_load_pass_the_client_servers_exactly() {
    let env = Env::new();
    let (client, backend) = (FakeClient::new(), FakeBackend::new());
    let factory = FakeFactory::returning(backend);
    let agent = env.agent(client, factory.clone());
    let servers = json!([stdio("docs")]);
    let request = serde_json::from_value(json!({"cwd": env.work(), "mcpServers": servers})).unwrap();
    agent.new_session(request).await.unwrap();
    let file = env.work().join("stored.jsonl");
    agent.store.upsert("stored", &env.work().display().to_string(), &file.display().to_string());
    let request = json!({"sessionId": "stored", "cwd": env.work(), "mcpServers": servers});
    agent.load_session(serde_json::from_value(request).unwrap()).await.unwrap();
    let recorded = factory.mcp_servers.lock().unwrap().clone();
    assert_eq!(recorded.len(), 2);
    recorded.iter().for_each(assert_literal);
    let map = std::fs::read_to_string(env.options().session_map_path).unwrap_or_default();
    assert!(!map.contains(SECRET), "{map}");
}

#[test]
fn debug_output_names_the_servers_only() {
    let servers = ClientMcpServers::from_request(&parsed(&json!([stdio("docs")])), None).unwrap();
    assert_eq!(format!("{servers:?}"), r#"["docs (5 args, 1 env)"]"#);
    assert_eq!(servers.names(), ["docs"]);
}

fn rejected_lists() -> Vec<(&'static str, Value)> {
    let headers = json!([{"name": "Authorization", "value": SECRET}]);
    let url = format!("https://mcp.example/{SECRET}");
    let twice = json!([{"name": "A", "value": SECRET}, {"name": "A", "value": SECRET}]);
    vec![
        ("HTTP transport is not supported", json!([{"type": "http", "name": "web", "url": url, "headers": headers}])),
        ("SSE transport is not supported", json!([{"type": "sse", "name": "web", "url": url, "headers": headers}])),
        ("absolute path", json!([{"name": "rel", "command": format!("rel/{SECRET}"), "args": [], "env": []}])),
        ("NUL", json!([{"name": "nul", "command": command(), "args": [format!("{SECRET}\u{0}")], "env": []}])),
        ("set more than once", json!([{"name": "dup", "command": command(), "args": [], "env": twice}])),
        ("the name is empty", json!([{"name": " ", "command": command(), "args": [SECRET], "env": []}])),
        ("would be the same", json!([stdio("a-b"), stdio("a.b")])),
    ]
}

#[tokio::test]
async fn invalid_lists_start_nothing_and_keep_the_live_session() {
    let env = Env::new();
    let (client, backend) = (FakeClient::new(), FakeBackend::new());
    let factory = FakeFactory::returning(backend.clone());
    let agent = env.agent(client, factory.clone());
    let request = serde_json::from_value(json!({"cwd": env.work(), "mcpServers": []})).unwrap();
    let response = agent.new_session(request).await.unwrap().response;
    let session_id = response["sessionId"].as_str().unwrap().to_string();
    for (expected, servers) in rejected_lists() {
        let new = json!({"cwd": env.work(), "mcpServers": servers});
        let load = json!({"sessionId": session_id, "cwd": env.work(), "mcpServers": servers});
        let errors = [
            agent.new_session(serde_json::from_value(new).unwrap()).await.err().unwrap(),
            agent.load_session(serde_json::from_value(load).unwrap()).await.err().unwrap(),
        ];
        for error in errors {
            let text = serde_json::to_string(&error).unwrap();
            assert_eq!(code(&error), -32602, "{text}");
            assert!(text.contains(expected), "{expected}: {text}");
            assert!(!text.contains(SECRET), "{text}");
        }
        assert_eq!(factory.spawn_count(), 1, "{expected}");
        assert!(agent.session(&session_id).is_some(), "{expected}");
        assert_eq!(backend.disposed.load(Ordering::SeqCst), 0, "{expected}");
    }
}

#[tokio::test]
async fn implicit_restores_do_not_bring_back_client_servers() {
    let env = Env::new();
    let (client, backend) = (FakeClient::new(), FakeBackend::new());
    let factory = FakeFactory::returning(backend);
    let agent = env.agent(client, factory.clone());
    let file = env.work().join("stored.jsonl");
    agent.store.upsert("stored", &env.work().display().to_string(), &file.display().to_string());
    let set_mode = || serde_json::from_value(json!({"sessionId": "stored", "modeId": "ask"})).unwrap();
    agent.set_session_mode(set_mode()).await.unwrap();
    agent.close("stored").await;
    let load = json!({"sessionId": "stored", "cwd": env.work(), "mcpServers": [stdio("docs")]});
    agent.load_session(serde_json::from_value(load).unwrap()).await.unwrap();
    agent.close("stored").await;
    agent.set_session_mode(set_mode()).await.unwrap();
    let recorded = factory.mcp_servers.lock().unwrap().clone();
    let names: Vec<Vec<&str>> = recorded.iter().map(ClientMcpServers::names).collect();
    assert_eq!(names, [vec![], vec!["docs"], vec![]]);
}

#[tokio::test]
async fn a_load_that_fails_after_the_restore_closes_the_session() {
    let env = Env::new();
    let config = FakeConfig { messages_error: Some("messages unavailable".into()), ..FakeConfig::default() };
    let (client, backend) = (FakeClient::new(), FakeBackend::with(config));
    let factory = FakeFactory::returning(backend.clone());
    let agent = env.agent(client, factory.clone());
    let file = env.work().join("stored.jsonl");
    agent.store.upsert("stored", &env.work().display().to_string(), &file.display().to_string());
    let load = json!({"sessionId": "stored", "cwd": env.work(), "mcpServers": [stdio("docs")]});
    let error = agent.load_session(serde_json::from_value(load).unwrap()).await.err().unwrap();
    assert_eq!(code(&error), -32603);
    assert!(!serde_json::to_string(&error).unwrap().contains(SECRET));
    assert_eq!(factory.mcp_servers.lock().unwrap()[0].names(), ["docs"]);
    assert!(agent.session("stored").is_none());
    assert_eq!(backend.disposed.load(Ordering::SeqCst), 1);
}
