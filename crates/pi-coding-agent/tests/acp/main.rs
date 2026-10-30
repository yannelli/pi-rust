//! ACP mode end to end: a scripted ACP client drives `serve_acp` over a byte stream against
//! the faux provider, and `pi --mode acp` over real stdio against a local mock server.

#[path = "../cli_support/mod.rs"]
mod cli_support;
#[path = "../agent_session/http_mock.rs"]
mod http_mock;
#[path = "../offline_guard/mod.rs"]
mod offline_guard;

mod binary;
mod client;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use pi_ai::auth::InMemoryCredentialStore;
use pi_ai::providers::faux::{FauxProvider, FauxResponse};
use pi_ai::register_api_provider;
use pi_coding_agent::acp::{AcpModeOptions, serve_acp};
use pi_coding_agent::app::AppOptions;
use pi_coding_agent::settings::SettingsManager;
use serde_json::{Value, json};

use client::{ChannelWriter, Client};

static NEXT_API: AtomicU64 = AtomicU64::new(1);

struct Setup {
    root: tempfile::TempDir,
    provider: Arc<FauxProvider>,
    options: AcpModeOptions,
}

/// An agent dir whose `local/tiny` model is served by a fresh faux provider.
fn setup(responses: Vec<FauxResponse>) -> Setup {
    let root = tempfile::tempdir().unwrap();
    let (agent_dir, work) = (root.path().join("agent"), root.path().join("work"));
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    let api = format!("faux-acp-{}", NEXT_API.fetch_add(1, Ordering::Relaxed));
    let models = json!({"providers": {"local": {"baseUrl": "http://unused", "api": api, "apiKey": "test-key",
        "models": [{"id": "tiny", "contextWindow": 128_000, "maxTokens": 4_000, "reasoning": true}]}}});
    std::fs::write(agent_dir.join("models.json"), models.to_string()).unwrap();
    offline_guard::assert_models_file_loads(&agent_dir.join("models.json"));
    // ACP refreshes model catalogs in the background unless offline.
    offline_guard::offline_process();
    let provider = FauxProvider::with_delay(responses, Duration::from_millis(2));
    register_api_provider(api, provider.clone());
    let settings = SettingsManager::in_memory(json!({"defaultProvider": "local", "defaultModel": "tiny"}));
    let app = AppOptions {
        agent_dir: agent_dir.clone(),
        settings: Some(Arc::new(settings)),
        credentials: Some(Arc::new(InMemoryCredentialStore::new(BTreeMap::new()))),
        credential_env: Some(offline_guard::credential_free_env()),
        ..AppOptions::default()
    };
    let mut options = AcpModeOptions::new(app, None);
    options.agent.home = root.path().to_path_buf();
    options.agent.session_map_path = root.path().join("pi-acp/session-map.json");
    options.agent.version = "9.9.9".into();
    Setup { root, provider, options }
}

fn start(setup: &Setup) -> Client<tokio::io::DuplexStream> {
    let (input, server_input) = tokio::io::duplex(1 << 20);
    let (writer, lines) = ChannelWriter::new();
    let options = setup.options.clone();
    tokio::spawn(async move { serve_acp(options, server_input, Box::new(writer)).await });
    Client::new(input, lines)
}

fn texts(updates: &[Value], kind: &str) -> String {
    updates
        .iter()
        .filter(|update| update["sessionUpdate"] == kind)
        .filter_map(|update| update["content"]["text"].as_str())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn drives_sessions_over_acp_with_the_faux_provider() {
    let setup = setup(vec![
        FauxResponse::tool_call(
            "call-1",
            "edit",
            json!({"path": "notes.txt", "edits": [{"oldText": "hello", "newText": "goodbye"}]}),
        ),
        FauxResponse::text("The note now says goodbye."),
        FauxResponse::Hang,
    ]);
    let work = setup.root.path().join("work");
    std::fs::write(work.join("notes.txt"), "first line\nhello\n").unwrap();
    let mut client = start(&setup);

    let init = client.request("initialize", json!({"protocolVersion": 1, "clientCapabilities": {}})).await;
    assert_eq!(init["result"]["protocolVersion"], 1);
    assert_eq!(init["result"]["agentCapabilities"]["sessionCapabilities"], json!({"list": {}, "delete": {}}));

    let created = client.request("session/new", json!({"cwd": work, "mcpServers": []})).await;
    let session_id = created["result"]["sessionId"].as_str().expect("session id").to_string();
    assert_eq!(created["result"]["models"]["currentModelId"], "local/tiny");
    assert!(created["result"]["_meta"]["piAcp"]["startupInfo"].as_str().unwrap().starts_with("pi v9.9.9"));
    let commands =
        client.until(|message| message["params"]["update"]["sessionUpdate"] == "available_commands_update").await;
    assert_eq!(commands["params"]["sessionId"], session_id.as_str());

    let prompt = json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "Change hello to goodbye"}]});
    let pending_prompt = client.send_request("session/prompt", prompt).await;
    let approval = client.until(|message| message["method"] == "session/request_permission").await;
    assert_eq!(approval["params"]["toolCall"]["toolCallId"], "call-1");
    assert_eq!(std::fs::read_to_string(work.join("notes.txt")).unwrap(), "first line\nhello\n");
    client.respond(approval["id"].clone(), json!({"outcome": {"outcome": "selected", "optionId": "allow"}})).await;
    let response = client.response(pending_prompt).await;
    assert_eq!(response["result"]["stopReason"], "end_turn");
    let updates = client.updates(&session_id);
    let chunks = updates.iter().filter(|update| update["sessionUpdate"] == "agent_message_chunk").count();
    assert!(chunks >= 2, "streamed in several chunks: {updates:?}");
    assert!(texts(&updates, "agent_message_chunk").ends_with("The note now says goodbye."));
    let call = updates.iter().find(|update| update["sessionUpdate"] == "tool_call").expect("tool_call");
    assert_eq!((call["toolCallId"].clone(), call["kind"].clone()), (json!("call-1"), json!("edit")));
    let notes = work.join("notes.txt").display().to_string();
    let started = updates
        .iter()
        .find(|update| update["toolCallId"] == "call-1" && update["status"] == "in_progress")
        .expect("execution start");
    assert_eq!(started["locations"], json!([{"path": notes, "line": 2}]));
    let done = updates
        .iter()
        .find(|update| update["toolCallId"] == "call-1" && update["status"] == "completed")
        .expect("completed");
    assert_eq!(
        done["content"],
        json!([{"type": "diff", "path": "notes.txt", "oldText": "first line\nhello\n", "newText": "first line\ngoodbye\n"}])
    );
    assert!(updates.iter().any(|update| update["sessionUpdate"] == "usage_update"));
    let title = updates
        .iter()
        .find(|update| update["sessionUpdate"] == "session_info_update" && update["title"].is_string())
        .expect("auto title");
    assert_eq!(title["title"], "Change hello to goodbye");

    let hang = json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "Wait forever"}]});
    let pending = client.send_request("session/prompt", hang).await;
    client.until(|message| message["params"]["update"]["_meta"]["piAcp"]["running"] == true).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    client.notify("session/cancel", json!({"sessionId": session_id})).await;
    let cancelled = client.response(pending).await;
    assert_eq!(cancelled["result"]["stopReason"], "cancelled");

    let listed = client.request("session/list", json!({})).await;
    let sessions = listed["result"]["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["sessionId"], session_id.as_str());
    assert_eq!(sessions[0]["title"], "Change hello to goodbye");

    client.clear();
    let loaded = client.request("session/load", json!({"sessionId": session_id, "cwd": work, "mcpServers": []})).await;
    assert_eq!(loaded["result"]["modes"]["currentModeId"], "auto");
    let replay = client.updates(&session_id);
    assert_eq!(texts(&replay, "user_message_chunk"), "Change hello to goodbyeWait forever");
    assert!(texts(&replay, "agent_message_chunk").contains("The note now says goodbye."));
    assert!(replay.iter().any(|update| update["sessionUpdate"] == "tool_call" && update["toolCallId"] == "call-1"));
    let deleted = client.request("session/delete", json!({"sessionId": session_id})).await;
    assert_eq!(deleted["result"], json!({}));
    assert_eq!(client.request("session/list", json!({})).await["result"]["sessions"], json!([]));
    assert_eq!(setup.provider.requests().len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn denied_tools_do_not_execute_and_bypass_is_explicit() {
    let setup = setup(vec![
        FauxResponse::tool_call("denied", "write", json!({"path": "output.txt", "content": "denied"})),
        FauxResponse::text("Permission denied."),
        FauxResponse::tool_call("allowed", "write", json!({"path": "output.txt", "content": "allowed"})),
        FauxResponse::text("Written."),
    ]);
    let work = setup.root.path().join("work");
    let mut client = start(&setup);
    let created = client.request("session/new", json!({"cwd": work, "mcpServers": []})).await;
    let id = created["result"]["sessionId"].clone();
    let prompt = json!({"sessionId": id, "prompt": [{"type": "text", "text": "Write the file"}]});
    let pending = client.send_request("session/prompt", prompt.clone()).await;
    let approval = client.until(|message| message["method"] == "session/request_permission").await;
    client.respond(approval["id"].clone(), json!({"outcome": {"outcome": "selected", "optionId": "reject"}})).await;
    assert_eq!(client.response(pending).await["result"]["stopReason"], "end_turn");
    assert!(!work.join("output.txt").exists());
    let changed = client.request("session/set_mode", json!({"sessionId": id, "modeId": "bypass"})).await;
    assert_eq!(changed["result"], json!({}));
    client.clear();
    assert_eq!(client.request("session/prompt", prompt).await["result"]["stopReason"], "end_turn");
    assert_eq!(std::fs::read_to_string(work.join("output.txt")).unwrap(), "allowed");
    assert!(!client.seen.iter().any(|message| message["method"] == "session/request_permission"));
}

#[tokio::test(flavor = "multi_thread")]
async fn reports_protocol_errors() {
    let setup = setup(Vec::new());
    let mut client = start(&setup);
    let unknown = client.request("session/fork", json!({})).await;
    assert_eq!(unknown["error"]["code"], -32601);
    assert_eq!(unknown["error"]["data"], json!({"method": "session/fork"}));
    let invalid = client.request("session/prompt", json!({"sessionId": 5})).await;
    assert_eq!(invalid["error"]["code"], -32602);
    let relative = client.request("session/new", json!({"cwd": "relative", "mcpServers": []})).await;
    assert_eq!(relative["error"]["code"], -32602);
    let missing = client.request("session/load", json!({"sessionId": "nope", "cwd": "/tmp", "mcpServers": []})).await;
    assert_eq!(missing["error"]["data"], "Unknown sessionId: nope");
    assert_eq!(client.request("authenticate", json!({"methodId": "pi_terminal_login"})).await["result"], json!({}));
}

/// Opens a `select` dialog as soon as it is bound to a session.
struct DialogHost {
    answer: Arc<std::sync::Mutex<Option<Option<String>>>>,
}

impl pi_coding_agent::rpc::RpcExtensionHost for DialogHost {
    fn bind(&self, binding: pi_coding_agent::rpc::RpcExtensionBinding) {
        use pi_coding_agent::extension_ui::{DialogOptions, ExtensionUi};
        let answer = self.answer.clone();
        tokio::spawn(async move {
            let options = ["Alpha".to_string(), "Beta".to_string()];
            let picked = binding.ui.select("Pick one", &options, DialogOptions::default()).await;
            *answer.lock().unwrap() = Some(picked);
        });
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn extension_dialogs_become_permission_requests() {
    let mut setup = setup(Vec::new());
    let answer = Arc::new(std::sync::Mutex::new(None));
    setup.options.extension_host = Some(Arc::new(DialogHost { answer: answer.clone() }));
    let mut client = start(&setup);
    let work = setup.root.path().join("work");
    let created = client.send_request("session/new", json!({"cwd": work, "mcpServers": []})).await;
    let request = client.until(|record| record["method"] == "session/request_permission").await;
    assert_eq!(request["params"]["toolCall"]["title"], "Pick one");
    assert_eq!(request["params"]["toolCall"]["rawInput"]["options"], json!(["Alpha", "Beta"]));
    client.respond(request["id"].clone(), json!({"outcome": {"outcome": "selected", "optionId": "choice-1"}})).await;
    assert!(client.response(created).await.get("result").is_some());
    for _ in 0..100 {
        if answer.lock().unwrap().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(*answer.lock().unwrap(), Some(Some("Beta".to_string())));
}
