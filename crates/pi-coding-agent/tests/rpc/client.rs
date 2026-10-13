//! `RpcClient` against the `pi` binary (upstream `rpc.test.ts`, `rpc-client-*.test.ts`).

use std::path::Path;
use std::time::Duration;

use pi_ai::ThinkingLevel;
use pi_coding_agent::agent_session::PromptDisposition;
use pi_coding_agent::rpc::{RpcClient, RpcClientOptions, RpcUiResponse, collect_until_settled};
use serde_json::json;

use crate::cli_support::{mock_server, setup, text_body};

fn client(root: &Path, args: &[&str]) -> RpcClient {
    RpcClient::new(RpcClientOptions {
        program: env!("CARGO_BIN_EXE_pi").into(),
        cwd: Some(root.join("work")),
        env: vec![
            ("PI_CODING_AGENT_DIR".into(), root.join("agent").display().to_string()),
            ("HOME".into(), root.display().to_string()),
        ],
        model: Some("mock-model".into()),
        args: args.iter().map(|arg| arg.to_string()).collect(),
        forward_stderr: false,
        ..RpcClientOptions::default()
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn drives_prompts_bash_and_session_commands() {
    let root = tempfile::tempdir().unwrap();
    let (url, _) = mock_server(vec![text_body("test123"), text_body("second")]).await;
    setup(root.path(), &url);
    let client = client(root.path(), &[]);
    client.start().await.unwrap();
    assert!(client.start().await.unwrap_err().to_string().contains("Client already started"));

    let state = client.get_state().await.unwrap();
    assert_eq!((state.model.unwrap().id, state.is_streaming, state.message_count), ("mock-model".into(), false, 0));
    assert_eq!(client.get_last_assistant_text().await.unwrap(), None);
    let events = client.prompt_and_wait("Reply with just: test123", Vec::new(), Duration::from_secs(20)).await.unwrap();
    assert!(events.iter().filter(|event| event["type"] == "message_end").count() >= 2);
    assert_eq!(events.last().unwrap()["type"], "agent_settled");
    assert_eq!(client.get_last_assistant_text().await.unwrap().as_deref(), Some("test123"));
    assert_eq!(client.get_messages().await.unwrap().len(), 3);

    let bash = client.bash("echo hello").await.unwrap();
    assert_eq!((bash.output.trim(), bash.exit_code, bash.cancelled), ("hello", Some(0), false));
    let entries = client.get_entries(None).await.unwrap();
    assert_eq!(entries.leaf_id.as_deref(), entries.entries.last().unwrap()["id"].as_str());
    let first = entries.entries[0]["id"].as_str().unwrap();
    assert_eq!(client.get_entries(Some(first)).await.unwrap().entries.len(), entries.entries.len() - 1);
    let missing = client.get_entries(Some("nonexistent-id")).await.unwrap_err();
    assert!(missing.to_string().contains("Entry not found"));
    let tree = client.get_tree().await.unwrap();
    assert_eq!((tree.roots.len(), tree.nodes.len(), tree.leaf_id.clone()), (1, entries.entries.len(), entries.leaf_id));

    client.set_session_name("my-test-session").await.unwrap();
    assert_eq!(client.get_state().await.unwrap().session_name.as_deref(), Some("my-test-session"));
    let stats = client.get_session_stats().await.unwrap();
    assert_eq!((stats.user_messages, stats.assistant_messages), (1, 1));
    assert!(stats.session_file.is_some() && stats.context_usage.is_some());
    let exported = client.export_html(None).await.unwrap();
    assert!(exported.ends_with(".html") && root.path().join("work").join(&exported).exists(), "{exported}");
    let commands = client.get_commands().await.unwrap();
    let names: Vec<&str> = commands.iter().map(|command| command.name.as_str()).collect();
    assert_eq!(names, ["mcp"], "only the built-in MCP command");
    assert!(client.get_available_models().await.unwrap().iter().any(|model| model.id == "mock-model"));
    client.set_thinking_level(ThinkingLevel::High).await.unwrap();
    assert_eq!(client.cycle_thinking_level().await.unwrap(), None, "the mock model has no reasoning");
    assert_eq!(client.get_available_thinking_levels().await.unwrap(), [ThinkingLevel::Off]);

    let original = client.get_state().await.unwrap().session_file.unwrap();
    let forkable = client.get_fork_messages().await.unwrap();
    assert_eq!(forkable[0].text, "Reply with just: test123");
    let forked = client.fork(&forkable[0].entry_id).await.unwrap();
    assert_eq!((forked.text.as_deref(), forked.cancelled), (Some("Reply with just: test123"), false));
    assert!(client.get_state().await.unwrap().message_count <= 1, "at most the system prompt");
    assert!(!client.new_session(None).await.unwrap());
    assert!(!client.switch_session(&original).await.unwrap());
    assert!(!client.clone_session().await.unwrap());
    let cloned = client.get_state().await.unwrap();
    assert_ne!(cloned.session_file.as_deref(), Some(original.as_str()));
    assert_eq!(cloned.message_count, 4, "system, user, assistant, and bash output");

    let mut events = client.subscribe();
    let disposition = client.prompt("again", Vec::new(), None).await.unwrap();
    assert_eq!(disposition, PromptDisposition::Started);
    collect_until_settled(&mut events, Duration::from_secs(20)).await.unwrap();
    assert_eq!(client.get_last_assistant_text().await.unwrap().as_deref(), Some("second"));
    client.stop().await;
    assert!(client.exit_error().is_some());
    assert!(client.get_state().await.unwrap_err().to_string().contains("Client not started"));
}

#[tokio::test(flavor = "multi_thread")]
async fn parses_session_trees_deeper_than_the_json_nesting_limit() {
    let root = tempfile::tempdir().unwrap();
    setup(root.path(), "http://127.0.0.1:9/v1");
    let client = client(root.path(), &["--no-session"]);
    client.start().await.unwrap();
    for index in 0..80 {
        client.bash_with(&format!("echo {index}"), true).await.unwrap();
    }
    let tree = client.get_tree().await.unwrap();
    assert_eq!(tree.roots.len(), 1);
    let mut depth = 0;
    let mut node = tree.roots[0];
    while let Some(child) = tree.nodes[node].children.first() {
        node = *child;
        depth += 1;
    }
    assert!(depth >= 80, "{depth}");
    assert_eq!(tree.entry_id(node), tree.leaf_id.as_deref());
    client.stop().await;
}

#[test]
fn builds_extension_ui_responses() {
    assert_eq!(
        RpcUiResponse::Value("Allow".into()).to_record("u1"),
        json!({"type": "extension_ui_response", "id": "u1", "value": "Allow"})
    );
    assert_eq!(
        RpcUiResponse::Confirmed(true).to_record("u2"),
        json!({"type": "extension_ui_response", "id": "u2", "confirmed": true})
    );
    assert_eq!(
        RpcUiResponse::Cancelled.to_record("u3"),
        json!({"type": "extension_ui_response", "id": "u3", "cancelled": true})
    );
}

/// An executable script written by a separate process, so no descriptor of this process can
/// keep it open for writing while another test thread forks (which fails `exec` with ETXTBSY).
#[cfg(unix)]
fn script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    let write = "printf '#!/bin/sh\\n%s\\n' \"$1\" > \"$2\" && chmod 755 \"$2\"";
    let status =
        std::process::Command::new("/bin/sh").args(["-c", write, "sh", body, path.to_str().unwrap()]).status().unwrap();
    assert!(status.success());
    path
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn rejects_requests_when_the_process_exits_or_stays_silent() {
    let dir = tempfile::tempdir().unwrap();
    let options = |program, timeout| RpcClientOptions {
        program,
        request_timeout: timeout,
        forward_stderr: false,
        ..RpcClientOptions::default()
    };
    let exits = RpcClient::new(options(
        script(dir.path(), "exits.sh", "read line\necho bye >&2\nexit 43"),
        Duration::from_secs(30),
    ));
    exits.start().await.unwrap();
    let error = exits.get_commands().await.unwrap_err().to_string();
    assert!(error.starts_with("Agent process exited (code=43 signal=null)") && error.contains("bye"), "{error}");
    assert_eq!(exits.stderr(), "bye\n");

    let silent =
        RpcClient::new(options(script(dir.path(), "silent.sh", "cat > /dev/null"), Duration::from_millis(200)));
    silent.start().await.unwrap();
    let error = silent.get_state().await.unwrap_err().to_string();
    assert!(error.starts_with("Timeout waiting for response to get_state"), "{error}");
    silent.respond_to_ui("u1", RpcUiResponse::Cancelled).await.unwrap();
    silent.stop().await;

    let early = RpcClient::new(options(script(dir.path(), "early.sh", "exit 3"), Duration::from_secs(30)));
    let error = early.start().await.unwrap_err().to_string();
    assert!(error.starts_with("Agent process exited (code=3"), "{error}");
}
