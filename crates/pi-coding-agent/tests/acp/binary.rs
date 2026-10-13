//! `pi --mode acp` as a subprocess over real stdio, with a local mock model server.

use std::path::Path;
use std::process::Stdio;

use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc::unbounded_channel;

use crate::cli_support::{mock_server, setup, text_body, tool_call_body};
use crate::client::Client;

/// `pi` with only the environment the test chooses, so ambient API keys stay out.
fn pi(root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pi"));
    command
        .args(args)
        .current_dir(root.join("work"))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", root)
        .env("USERPROFILE", root)
        .env("PI_CODING_AGENT_DIR", root.join("agent"))
        .env("PI_OFFLINE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    command
}

fn spawn(root: &Path, args: &[&str]) -> (Child, Client<ChildStdin>) {
    let mut child = pi(root, args).spawn().unwrap();
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (sender, lines) = unbounded_channel();
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            let _ = sender.send(line);
        }
    });
    (child, Client::new(stdin, lines))
}

async fn exit(mut child: Child, client: Client<ChildStdin>) -> Option<i32> {
    drop(client);
    let status = tokio::time::timeout(std::time::Duration::from_secs(20), child.wait()).await;
    status.expect("exits once stdin closes").unwrap().code()
}

#[tokio::test(flavor = "multi_thread")]
async fn serves_acp_over_stdio() {
    let root = tempfile::tempdir().unwrap();
    let (url, requests) =
        mock_server(vec![tool_call_body("call-1", "read", "{\"path\":\"notes.txt\"}"), text_body("The notes say hi.")])
            .await;
    setup(root.path(), &url);
    std::fs::write(root.path().join("work/notes.txt"), "hi\n").unwrap();
    let work = root.path().join("work");
    let (child, mut client) = spawn(root.path(), &["--mode", "acp", "--model", "mock-model"]);

    let init = client.request("initialize", json!({"protocolVersion": 1})).await;
    assert_eq!(init["result"]["authMethods"][0]["type"], "terminal");
    let created = client.request("session/new", json!({"cwd": work, "mcpServers": []})).await;
    let session_id = created["result"]["sessionId"].as_str().expect("session id").to_string();
    assert_eq!(created["result"]["models"]["currentModelId"], "mock/mock-model");

    let prompt = json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "What do the notes say?"}]});
    let response = client.request("session/prompt", prompt).await;
    assert_eq!(response["result"]["stopReason"], "end_turn");
    let updates = client.updates(&session_id);
    let read = updates.iter().find(|update| update["sessionUpdate"] == "tool_call").expect("tool_call");
    assert_eq!((read["toolCallId"].clone(), read["kind"].clone()), (json!("call-1"), json!("read")));
    let notes = work.join("notes.txt").display().to_string();
    assert!(updates.iter().any(|update| update["locations"] == json!([{"path": notes}])));
    let text: String = updates
        .iter()
        .filter(|update| update["sessionUpdate"] == "agent_message_chunk")
        .filter_map(|update| update["content"]["text"].as_str())
        .collect();
    assert!(text.ends_with("The notes say hi."), "{text}");
    assert_eq!(requests.lock().unwrap().len(), 2);
    assert_eq!(exit(child, client).await, Some(0));

    let map = std::fs::read_to_string(root.path().join(".pi/pi-acp/session-map.json")).unwrap();
    assert!(map.contains(&session_id), "{map}");
    let (child, mut client) = spawn(root.path(), &["--mode", "acp", "--model", "mock-model"]);
    let listed = client.request("session/list", json!({"cwd": work})).await;
    assert_eq!(listed["result"]["sessions"][0]["sessionId"], session_id.as_str());
    let loaded = client.request("session/load", json!({"sessionId": session_id, "cwd": work, "mcpServers": []})).await;
    assert!(loaded.get("result").is_some(), "{loaded}");
    let replay = client.updates(&session_id);
    assert!(replay.iter().any(|update| update["sessionUpdate"] == "user_message_chunk"
        && update["content"]["text"] == "What do the notes say?"));
    assert_eq!(exit(child, client).await, Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn new_session_requires_auth_without_models() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("agent")).unwrap();
    std::fs::create_dir_all(root.path().join("work")).unwrap();
    let (child, mut client) = spawn(root.path(), &["--mode", "acp"]);
    let created = client.request("session/new", json!({"cwd": root.path().join("work"), "mcpServers": []})).await;
    assert_eq!(created["error"]["code"], -32000, "{created}");
    assert_eq!(
        created["error"]["message"],
        "Authentication required: Configure an API key or log in with an OAuth provider."
    );
    assert_eq!(created["error"]["data"]["authMethods"][0]["id"], "pi_terminal_login");
    assert_eq!(exit(child, client).await, Some(0));
}
