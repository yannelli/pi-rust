//! Permission requests name the MCP tool behind a call by its server's own names, and the calling
//! tool for nested calls. `_meta.piAcp.clientMcpExposure: "direct"` offers client tools directly.

use std::sync::{Arc, Mutex};

use pi_agent::{AgentTool, AgentToolResult, ToolAnnotations, ToolUpdateCallback};
use pi_ai::providers::faux::FauxResponse;
use pi_coding_agent::agent_session::{AgentSession, ToolSource};
use pi_coding_agent::rpc::{RpcExtensionBinding, RpcExtensionHost};
use pi_mcp::create_mcp_tool_name;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::client::Client;
use crate::mcp_fake::{Fake, FakeOptions};
use crate::{Setup, setup, start};

fn command() -> String {
    std::env::temp_dir().join("acp-identity-server").display().to_string()
}

fn servers(names: &[&str]) -> Value {
    names.iter().map(|name| json!({"name": name, "command": command(), "args": [], "env": []})).collect()
}

fn with_fake(setup: &mut Setup) -> Fake {
    let fake = Fake::new(FakeOptions::default());
    setup.options.app.mcp.create_transport = Some(fake.factory());
    fake
}

async fn open(client: &mut Client<tokio::io::DuplexStream>, work: &std::path::Path, request: Value) -> String {
    client.request("initialize", json!({"protocolVersion": 1, "clientCapabilities": {}})).await;
    let mut params = request;
    params["cwd"] = json!(work);
    let created = client.request("session/new", params).await;
    let id = created["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{created}")).to_string();
    client.request("session/set_mode", json!({"sessionId": id, "modeId": "ask"})).await;
    id
}

/// Prompt once, answering each permission request with `answer`; returns the requests in order.
async fn prompt(
    client: &mut Client<tokio::io::DuplexStream>,
    id: &str,
    answer: impl Fn(&Value) -> &'static str,
) -> Vec<Value> {
    let prompt = json!({"sessionId": id, "prompt": [{"type": "text", "text": "go"}]});
    let pending = client.send_request("session/prompt", prompt).await;
    let mut requests = Vec::new();
    loop {
        let next = client
            .until(|m| m["method"] == "session/request_permission" || (m["id"] == pending && m.get("method").is_none()))
            .await;
        if next["method"] != "session/request_permission" {
            assert_eq!(next["result"]["stopReason"], "end_turn", "{next}");
            return requests;
        }
        let option = answer(&next["params"]);
        client.respond(next["id"].clone(), json!({"outcome": {"outcome": "selected", "optionId": option}})).await;
        requests.push(next["params"].clone());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn direct_client_tools_ask_with_their_exact_server_and_tool_names() {
    let long = "x".repeat(70);
    let names = ["a-b.c d", "é docs", long.as_str()];
    let tools: Vec<String> = names.iter().map(|name| create_mcp_tool_name(name, "search", |_| false)).collect();
    assert!(tools[2].len() == 64 && !tools[2].contains(&long), "{}", tools[2]);
    let mut calls: Vec<FauxResponse> = tools
        .iter()
        .enumerate()
        .map(|(i, tool)| FauxResponse::tool_call(&format!("t{i}"), tool, json!({"query": "q"})))
        .collect();
    calls.push(FauxResponse::tool_call("t3", "mcp__cfg__search", json!({"query": "direct"})));
    let code = "await tools.mcp__cfg__search({ query: \"cfg\" });";
    calls.push(FauxResponse::tool_call("t4", "codemode", json!({ "code": code })));
    calls.push(FauxResponse::text("done"));
    let mut setup = setup(calls);
    let fake = with_fake(&mut setup);
    let (root, work) = (setup.root.path().to_path_buf(), setup.root.path().join("work"));
    std::fs::write(root.join("agent/mcp.json"), json!({"mcpServers": {"cfg": {"command": "/bin/never"}}}).to_string())
        .unwrap();
    let mut client = start(&setup);
    let request = json!({"mcpServers": servers(&names), "_meta": {"piAcp": {"clientMcpExposure": "direct"}}});
    let id = open(&mut client, &work, request).await;
    let requests = prompt(&mut client, &id, |_| "allow").await;
    let identities: Vec<Value> = requests.iter().map(|request| request["toolCall"]["_meta"]["piAcp"].clone()).collect();
    let mut expected: Vec<Value> =
        names.iter().map(|name| json!({"mcp": {"server": name, "tool": "search"}})).collect();
    expected.push(Value::Null);
    expected.push(json!({"mcp": {"server": "cfg", "tool": "search"}, "parentToolCallId": "t4"}));
    // The configured server connected with codemode exposure: no direct call, one call through codemode.
    assert_eq!(identities, expected);
    let mut connected = fake.connected.lock().unwrap().clone();
    connected.sort();
    let mut servers: Vec<String> = names.iter().map(|name| name.to_string()).chain(["cfg".to_string()]).collect();
    servers.sort();
    assert_eq!(connected, servers);
    let q = r#"search:{"query":"q"}"#;
    assert_eq!(fake.calls(), [q, q, q, r#"search:{"query":"cfg"}"#]);
    let titles: Vec<&str> = requests.iter().filter_map(|request| request["toolCall"]["title"].as_str()).collect();
    assert_eq!(titles[..3], tools);
    assert_eq!(titles[3..], ["codemode", "mcp__cfg__search"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn nested_codemode_calls_each_ask_and_name_the_codemode_call() {
    let code = "await tools.mcp__docs__search({ query: \"a\" }); await tools.mcp__docs__search({ query: \"b\" });";
    let mut setup =
        setup(vec![FauxResponse::tool_call("c1", "codemode", json!({ "code": code })), FauxResponse::text("done")]);
    let fake = with_fake(&mut setup);
    let work = setup.root.path().join("work");
    let mut client = start(&setup);
    let id = open(&mut client, &work, json!({"mcpServers": servers(&["docs"])})).await;
    let requests = prompt(&mut client, &id, |request| {
        let second = request["toolCall"]["rawInput"]["query"] == "b";
        if second { "reject" } else { "allow" }
    })
    .await;
    let metas: Vec<Value> = requests.iter().map(|request| request["toolCall"]["_meta"].clone()).collect();
    let nested = json!({"piAcp": {"mcp": {"server": "docs", "tool": "search"}, "parentToolCallId": "c1"}});
    assert_eq!(metas, [Value::Null, nested.clone(), nested]);
    assert_eq!(requests[0]["toolCall"]["title"], "codemode");
    // The rejected nested call never reached the server; the allowed one did.
    assert_eq!(fake.calls(), [r#"search:{"query":"a"}"#]);
}

/// Keeps the bound session, so a test can change its tools while a call waits for approval.
#[derive(Default)]
struct SessionHost {
    session: Mutex<Option<Arc<AgentSession>>>,
}

impl RpcExtensionHost for SessionHost {
    fn bind(&self, binding: RpcExtensionBinding) {
        *self.session.lock().unwrap() = Some(binding.session.clone());
    }
}

/// `probe`, answering with `reply`.
struct Probe {
    reply: &'static str,
    annotations: ToolAnnotations,
}

fn probe(reply: &'static str, read_only: bool) -> Arc<Probe> {
    Arc::new(Probe { reply, annotations: ToolAnnotations { read_only_hint: Some(read_only), ..Default::default() } })
}

#[async_trait::async_trait]
impl AgentTool for Probe {
    fn name(&self) -> &str {
        "probe"
    }
    fn description(&self) -> &str {
        "Probe."
    }
    fn parameters(&self) -> Value {
        json!({"type": "object"})
    }
    fn annotations(&self) -> Option<&ToolAnnotations> {
        Some(&self.annotations)
    }
    async fn execute(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
        _: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        Ok(AgentToolResult::text(self.reply))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_mode_asks_by_the_annotations_of_the_tool_that_runs() {
    for captured_read_only in [false, true] {
        let gate = ("w", "write", json!({"path": "gate.txt", "content": "x"}));
        let mut setup =
            setup(vec![FauxResponse::tool_calls(&[gate, ("p", "probe", json!({}))]), FauxResponse::text("done")]);
        let host = Arc::new(SessionHost::default());
        setup.options.extension_host = Some(host.clone());
        let work = setup.root.path().join("work");
        let mut client = start(&setup);
        client.request("initialize", json!({"protocolVersion": 1, "clientCapabilities": {}})).await;
        let created = client.request("session/new", json!({"cwd": work, "mcpServers": []})).await;
        let id = created["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{created}")).to_string();
        let session = host.session.lock().unwrap().clone().expect("bound session");
        session.register_tool(probe("captured", captured_read_only), ToolSource::sdk("acp-test"));
        let prompt = json!({"sessionId": id, "prompt": [{"type": "text", "text": "go"}]});
        let pending = client.send_request("session/prompt", prompt).await;
        let write = client.until(|m| m["method"] == "session/request_permission").await;
        assert_eq!(write["params"]["toolCall"]["title"], "write");
        // The turn resolved `probe` before this request; the name now belongs to the other annotation.
        session.register_tool(probe("replacement", !captured_read_only), ToolSource::sdk("acp-test"));
        client.respond(write["id"].clone(), json!({"outcome": {"outcome": "selected", "optionId": "reject"}})).await;
        let next = client
            .until(|m| m["method"] == "session/request_permission" || (m["id"] == pending && m.get("method").is_none()))
            .await;
        if captured_read_only {
            assert_eq!(next["result"]["stopReason"], "end_turn", "{next}");
        } else {
            assert_eq!(next["params"]["toolCall"]["title"], "probe", "{next}");
            client.respond(next["id"].clone(), json!({"outcome": {"outcome": "selected", "optionId": "allow"}})).await;
            assert_eq!(client.response(pending).await["result"]["stopReason"], "end_turn");
        }
        let probe_updates: Vec<Value> =
            client.updates(&id).into_iter().filter(|update| update["toolCallId"] == "p").collect();
        let text = serde_json::to_string(&probe_updates).unwrap();
        assert!(text.contains("captured") && !text.contains("replacement"), "{text}");
        assert!(!work.join("gate.txt").exists());
    }
}
