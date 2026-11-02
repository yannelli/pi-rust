//! MCP servers the ACP client supplies: their tools work after `session/new` and `session/load`, the
//! values reach the transport exactly as sent, and none of them is written to disk.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_ai::providers::faux::FauxResponse;
use pi_mcp::runtime::TransportFactory;
use pi_mcp::{McpConfigScope, McpServerEntry};
use serde_json::{Value, json};

use crate::mcp_fake::{Fake, FakeOptions};
use crate::{Setup, setup, setup_with_models, start};

const SECRET: &str = "SENTINEL_ACP_9c1e";

fn command() -> String {
    std::env::temp_dir().join("acp-client-docs").display().to_string()
}

fn client_servers() -> Value {
    json!([{"name": "docs", "command": command(), "args": ["~/x", "$HOME", "!echo hi", SECRET],
        "env": [{"name": "TOKEN", "value": format!("!cat ${{HOME}} ~/u {SECRET}")}]}])
}

/// The fake MCP server for every session, recording each entry the transport factory gets.
fn with_fake(setup: &mut Setup) -> (Fake, Arc<Mutex<Vec<McpServerEntry>>>) {
    let fake = Fake::new(FakeOptions::default());
    let entries: Arc<Mutex<Vec<McpServerEntry>>> = Arc::default();
    let (seen, inner) = (entries.clone(), fake.factory());
    let factory: TransportFactory = Arc::new(move |entry, cwd, auth| {
        seen.lock().unwrap().push(entry.clone());
        inner(entry, cwd, auth)
    });
    setup.options.app.mcp.create_transport = Some(factory);
    (fake, entries)
}

fn search(id: &str) -> FauxResponse {
    let code = "const found = await tools.mcp__docs__search({ query: \"acp\" }); text(found.content[0].text);";
    FauxResponse::tool_call(id, "codemode", json!({ "code": code }))
}

/// Every file under `dir`, read as text.
fn all_text(dir: &std::path::Path) -> String {
    let mut text = String::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            text.push_str(&all_text(&path));
        } else {
            text.push_str(&String::from_utf8_lossy(&std::fs::read(&path).unwrap_or_default()));
        }
    }
    text
}

async fn closed_eventually(fake: &Fake) {
    for _ in 0..200 {
        if fake.servers.lock().unwrap().iter().all(|server| server.is_closed()) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("an MCP transport of a closed session is still open");
}

#[tokio::test(flavor = "multi_thread")]
async fn client_servers_work_after_new_and_load_and_stay_off_disk() {
    let mut setup = setup(vec![search("c1"), FauxResponse::text("found"), search("c2"), FauxResponse::text("again")]);
    let (fake, entries) = with_fake(&mut setup);
    let (root, work) = (setup.root.path().to_path_buf(), setup.root.path().join("work"));
    let agent_config = json!({"mcpServers": {"docs": {"url": "http://config.invalid"}}}).to_string();
    std::fs::write(root.join("agent/mcp.json"), &agent_config).unwrap();
    std::fs::create_dir_all(work.join(".pi")).unwrap();
    let project_config = json!({"mcpServers": {"project-only": {"command": "/bin/never"}}}).to_string();
    std::fs::write(work.join(".pi/mcp.json"), &project_config).unwrap();
    let mut client = start(&setup);
    client.request("initialize", json!({"protocolVersion": 1, "clientCapabilities": {}})).await;

    let created = client.request("session/new", json!({"cwd": work, "mcpServers": client_servers()})).await;
    let id = created["result"]["sessionId"].as_str().expect("session id").to_string();
    for round in ["first", "after load"] {
        client.request("session/set_mode", json!({"sessionId": id, "modeId": "bypass"})).await;
        let prompt = json!({"sessionId": id, "prompt": [{"type": "text", "text": "search"}]});
        let response = client.request("session/prompt", prompt).await;
        assert_eq!(response["result"]["stopReason"], "end_turn", "{round}: {response}");
        if round == "first" {
            let load = json!({"sessionId": id, "cwd": work, "mcpServers": client_servers()});
            let loaded = client.request("session/load", load).await;
            assert!(loaded["result"].is_object(), "{loaded}");
        }
    }
    assert_eq!(fake.calls(), [r#"search:{"query":"acp"}"#, r#"search:{"query":"acp"}"#]);

    // The client's "docs" replaced mcp.json's "docs" and got every value as sent. The project, which
    // the test settings trust, still adds its own server.
    let entries = entries.lock().unwrap().clone();
    let docs: Vec<&McpServerEntry> = entries.iter().filter(|entry| entry.name == "docs").collect();
    assert_eq!(docs.len(), 2, "{entries:?}");
    let others: Vec<(&str, Option<McpConfigScope>)> =
        entries.iter().filter(|entry| entry.name != "docs").map(|entry| (entry.name.as_str(), entry.scope)).collect();
    assert_eq!(others, [("project-only", Some(McpConfigScope::Project)); 2]);
    for entry in docs {
        assert_eq!((entry.name.as_str(), entry.scope), ("docs", Some(McpConfigScope::Client)));
        assert_eq!(entry.config.command.as_deref(), Some(command().as_str()));
        let args: Vec<String> = ["~/x", "$HOME", "!echo hi", SECRET].map(String::from).into();
        assert_eq!(entry.config.args.as_ref(), Some(&args));
        let token = entry.config.env.as_ref().and_then(|env| env.get("TOKEN")).cloned();
        assert_eq!(token, Some(json!(format!("!cat ${{HOME}} ~/u {SECRET}"))));
    }
    assert_eq!(std::fs::read_to_string(root.join("agent/mcp.json")).unwrap(), agent_config);
    assert_eq!(std::fs::read_to_string(work.join(".pi/mcp.json")).unwrap(), project_config);
    for (place, text) in [
        ("agent dir", all_text(&root.join("agent"))),
        ("session map", all_text(&root.join("pi-acp"))),
        ("messages to the client", serde_json::to_string(&client.seen).unwrap()),
    ] {
        assert!(!text.contains(SECRET), "{place} holds a client value");
    }

    // A bad list starts nothing; the session from `session/load` stays usable.
    let rejected =
        json!([{"type": "http", "name": "web", "url": format!("https://x.invalid/{SECRET}"), "headers": []}]);
    let error = client.request("session/load", json!({"sessionId": id, "cwd": work, "mcpServers": rejected})).await;
    assert_eq!(error["error"]["code"], -32602, "{error}");
    assert!(!error.to_string().contains(SECRET), "{error}");
    assert_eq!(fake.connected.lock().unwrap().len(), entries.len());
    let closed = client.request("session/delete", json!({"sessionId": id})).await;
    assert_eq!(closed["result"], json!({}));
}

#[tokio::test(flavor = "multi_thread")]
async fn client_servers_work_in_an_untrusted_project_whose_mcp_json_stays_unloaded() {
    let mut setup = setup(vec![search("c1"), FauxResponse::text("found")]);
    let (fake, entries) = with_fake(&mut setup);
    setup.options.app.settings.as_ref().expect("settings").set_project_trusted(false);
    let work = setup.root.path().join("work");
    std::fs::create_dir_all(work.join(".pi")).unwrap();
    let project_config = json!({"mcpServers": {"project-only": {"command": "/bin/never"}}}).to_string();
    std::fs::write(work.join(".pi/mcp.json"), &project_config).unwrap();
    let mut client = start(&setup);
    client.request("initialize", json!({"protocolVersion": 1, "clientCapabilities": {}})).await;
    let created = client.request("session/new", json!({"cwd": work, "mcpServers": client_servers()})).await;
    let id = created["result"]["sessionId"].as_str().expect("session id").to_string();
    client.request("session/set_mode", json!({"sessionId": id, "modeId": "bypass"})).await;
    let prompt = json!({"sessionId": id, "prompt": [{"type": "text", "text": "search"}]});
    let response = client.request("session/prompt", prompt).await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(fake.calls(), [r#"search:{"query":"acp"}"#]);
    let started: Vec<(String, Option<McpConfigScope>)> =
        entries.lock().unwrap().iter().map(|entry| (entry.name.clone(), entry.scope)).collect();
    assert_eq!(started, [("docs".to_string(), Some(McpConfigScope::Client))]);
    assert_eq!(std::fs::read_to_string(work.join(".pi/mcp.json")).unwrap(), project_config);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_new_session_closes_its_client_servers() {
    let mut setup = setup_with_models(Vec::new(), json!([]));
    let (fake, _) = with_fake(&mut setup);
    let work = setup.root.path().join("work");
    let mut client = start(&setup);
    client.request("initialize", json!({"protocolVersion": 1, "clientCapabilities": {}})).await;
    let created = client.request("session/new", json!({"cwd": work, "mcpServers": client_servers()})).await;
    assert!(created["error"].is_object(), "{created}");
    assert!(!created.to_string().contains(SECRET), "{created}");
    closed_eventually(&fake).await;
    assert!(!all_text(&setup.root.path().join("agent")).contains(SECRET));
}
