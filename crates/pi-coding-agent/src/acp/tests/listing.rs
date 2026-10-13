//! Upstream `session-list-*`, `session-title-long-session`, `session-updatedAt-message-only`,
//! `session-load-toolresult`, `session-restored-title`, `session-delete`, and the session map.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};

use super::{Env, FakeBackend, FakeClient, FakeConfig, FakeFactory, run_after, settle};
use crate::acp::listing::list_pi_sessions;
use crate::acp::settings::sessions_root;
use crate::acp::store::SessionStore;

fn write_session(path: &Path, entries: &[Value]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let text: Vec<String> = entries.iter().map(Value::to_string).collect();
    std::fs::write(path, text.join("\n") + "\n").unwrap();
}

fn header(id: &str, cwd: &str) -> Value {
    json!({"type": "session", "version": 3, "id": id, "timestamp": "2026-01-01T00:00:00.000Z", "cwd": cwd})
}

fn message(role: &str, content: Value, timestamp: &str) -> Value {
    json!({"type": "message", "id": "m", "parentId": null, "timestamp": timestamp, "message": {"role": role, "content": content}})
}

fn info(name: &str, timestamp: &str) -> Value {
    json!({"type": "session_info", "id": "i", "parentId": null, "timestamp": timestamp, "name": name})
}

#[tokio::test]
async fn lists_sessions_and_load_replays_history() {
    let env = Env::new();
    let file = env.agent_dir().join("sessions/--tmp--project--/0000_aaaa.jsonl");
    write_session(
        &file,
        &[
            header("sess-1", "/tmp/project"),
            message("user", json!("Hello"), "2026-02-11T00:00:01.000Z"),
            message("assistant", json!([{"type": "text", "text": "Hi there!"}]), "2026-02-11T00:00:02.000Z"),
            info("My Named Session", "2026-02-11T00:00:03.000Z"),
        ],
    );
    let config = FakeConfig {
        messages: json!({"messages": [{"role": "user", "content": "Hello"},
            {"role": "assistant", "content": [{"type": "text", "text": "Hi there!"}]}]}),
        stats: Box::new(|_| Ok(json!({"contextUsage": {"tokens": 12_345, "contextWindow": 200_000}}))),
        ..FakeConfig::default()
    };
    let (client, backend) = (FakeClient::new(), FakeBackend::with(config));
    let factory = FakeFactory::returning(backend);
    let agent = env.agent(client.clone(), factory.clone());
    let listed = serde_json::to_value(agent.list_sessions(serde_json::from_value(json!({})).unwrap())).unwrap();
    assert_eq!(
        listed,
        json!({"sessions": [{"sessionId": "sess-1", "cwd": "/tmp/project", "title": "My Named Session",
            "updatedAt": "2026-02-11T00:00:02.000Z"}], "_meta": {}})
    );
    let request =
        serde_json::from_value(json!({"sessionId": "sess-1", "cwd": "/tmp/project", "mcpServers": []})).unwrap();
    let handled = agent.load_session(request).await.unwrap();
    assert_eq!(factory.spawns.lock().unwrap()[0].1.as_deref(), Some(file.as_path()));
    let bodies = client.update_bodies();
    assert_eq!(bodies[0], json!({"sessionUpdate": "session_info_update", "title": "My Named Session"}));
    assert_eq!(bodies[1], json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "Hello"}}));
    assert_eq!(
        bodies[2],
        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Hi there!"}})
    );
    assert!(client.of_kind("usage_update").is_empty());
    run_after(handled).await;
    assert_eq!(
        client.of_kind("usage_update"),
        [json!({"sessionUpdate": "usage_update", "used": 12_345, "size": 200_000})]
    );
    assert_eq!(agent.store.get("sess-1").unwrap().session_file, file.display().to_string());

    let request =
        serde_json::from_value(json!({"sessionId": "missing", "cwd": "/tmp/project", "mcpServers": []})).unwrap();
    let error = agent.load_session(request).await.err().unwrap();
    assert_eq!(crate::acp::errors::code(&error), -32602);
}

#[tokio::test]
async fn load_replays_tool_results_as_tool_calls() {
    let env = Env::new();
    let config = FakeConfig {
        messages: json!({"messages": [
            {"role": "toolResult", "toolCallId": "call_1", "toolName": "bash", "args": {"command": "echo hello"},
                "content": [{"type": "text", "text": "hello from bash"}], "isError": false},
            {"role": "toolResult", "toolCallId": "call_2", "toolName": "read", "content": [{"type": "text", "text": "file"}],
                "isError": true},
        ]}),
        ..FakeConfig::default()
    };
    let (client, backend) = (FakeClient::new(), FakeBackend::with(config));
    let agent = env.agent(client.clone(), FakeFactory::returning(backend));
    agent.store.upsert("s1", "/tmp/project", "/tmp/s.jsonl");
    let request = serde_json::from_value(json!({"sessionId": "s1", "cwd": "/tmp/project", "mcpServers": []})).unwrap();
    agent.load_session(request).await.unwrap();
    let bodies = client.update_bodies();
    assert_eq!(
        bodies[0],
        json!({"sessionUpdate": "tool_call", "toolCallId": "call_1", "title": "echo hello", "kind": "execute",
            "status": "completed", "content": [{"type": "terminal", "terminalId": "call_1"}],
            "_meta": {"terminal_info": {"terminal_id": "call_1", "cwd": "/tmp/project"}}})
    );
    assert_eq!(
        bodies[1],
        json!({"sessionUpdate": "tool_call_update", "toolCallId": "call_1", "status": "completed",
            "_meta": {"terminal_output": {"terminal_id": "call_1", "data": "hello from bash"},
                "terminal_exit": {"terminal_id": "call_1", "exit_code": 0, "signal": null}}})
    );
    assert_eq!(
        (bodies[2]["kind"].clone(), bodies[2]["rawOutput"]["toolCallId"].clone()),
        (json!("read"), json!("call_2"))
    );
    assert_eq!(bodies[3]["status"], "failed");
    assert_eq!(bodies[3]["content"], json!([{"type": "content", "content": {"type": "text", "text": "file"}}]));
}

#[test]
fn listing_defaults_to_the_last_session_cwd_and_pages() {
    let env = Env::new();
    let sessions = env.agent_dir().join("sessions");
    write_session(
        &sessions.join("--a--/1.jsonl"),
        &[header("sess-a", "/cwd/a"), info("A", "2026-01-01T00:00:01.000Z")],
    );
    write_session(
        &sessions.join("--b--/2.jsonl"),
        &[header("sess-b", "/cwd/b"), info("B", "2026-01-01T00:00:01.000Z")],
    );
    let agent = env.agent(FakeClient::new(), FakeFactory::returning(FakeBackend::new()));
    let all = agent.list_sessions(serde_json::from_value(json!({})).unwrap());
    assert_eq!(all.sessions.len(), 2);
    agent.set_last_cwd("/cwd/a");
    let scoped = agent.list_sessions(serde_json::from_value(json!({})).unwrap());
    assert_eq!(scoped.sessions.iter().map(|s| s.session_id.to_string()).collect::<Vec<_>>(), ["sess-a"]);
    let explicit = agent.list_sessions(serde_json::from_value(json!({"cwd": "/cwd/b"})).unwrap());
    assert_eq!(explicit.sessions[0].session_id.to_string(), "sess-b");

    for index in 0..55 {
        let entries =
            [header(&format!("p-{index}"), "/cwd/p"), message("user", json!("hi"), "2026-01-01T00:00:01.000Z")];
        write_session(&sessions.join(format!("--p--/{index}.jsonl")), &entries);
    }
    let first = agent.list_sessions(serde_json::from_value(json!({"cwd": "/cwd/p"})).unwrap());
    assert_eq!((first.sessions.len(), first.next_cursor.as_deref()), (50, Some("50")));
    let second = agent.list_sessions(serde_json::from_value(json!({"cwd": "/cwd/p", "cursor": "50"})).unwrap());
    assert_eq!((second.sessions.len(), second.next_cursor), (5, None));
}

#[test]
fn titles_and_update_times_follow_pi_resume_rules() {
    let root = tempfile::tempdir().unwrap();
    let filler: Vec<Value> =
        (0..400).map(|_| message("user", json!("x".repeat(2000)), "2026-01-01T00:00:02.000Z")).collect();
    let mut long = vec![header("sess-long", "/tmp/project"), info("Named Early", "2026-01-01T00:00:01.000Z")];
    long.extend(filler);
    write_session(&root.path().join("--p--/long.jsonl"), &long);
    write_session(
        &root.path().join("--p--/times.jsonl"),
        &[
            header("sess-times", "/tmp/project"),
            message("user", json!("hi"), "2026-01-01T00:00:02.000Z"),
            info("named", "2026-01-01T00:00:10.000Z"),
        ],
    );
    write_session(
        &root.path().join("--p--/untitled.jsonl"),
        &[
            header("sess-untitled", "/tmp/project"),
            message("user", json!([{"type": "text", "text": "  First\n question "}]), "2026-01-01T00:00:03.000Z"),
        ],
    );
    std::fs::write(root.path().join("--p--/not-a-session.jsonl"), "{\"type\":\"message\"}\n").unwrap();
    let items = list_pi_sessions(root.path());
    let find = |id: &str| items.iter().find(|item| item.session_id == id).unwrap().clone();
    assert_eq!(find("sess-long").title.as_deref(), Some("Named Early"));
    assert_eq!(find("sess-times").updated_at.as_deref(), Some("2026-01-01T00:00:02.000Z"));
    assert_eq!(find("sess-untitled").title.as_deref(), Some("First question"));
    assert_eq!(items.len(), 3);
    assert_eq!(items[0].session_id, "sess-untitled", "most recent first");
}

#[test]
fn session_dir_setting_moves_the_listing_root() {
    let root = tempfile::tempdir().unwrap();
    let custom = root.path().join("somewhere-else");
    std::fs::write(root.path().join("settings.json"), json!({"sessionDir": custom}).to_string()).unwrap();
    let file = custom.join("--p--/s.jsonl");
    write_session(
        &file,
        &[header("sess-custom", "/tmp/project"), message("user", json!("hi"), "2026-01-01T00:00:01.000Z")],
    );
    let found = list_pi_sessions(&sessions_root(root.path(), None));
    assert_eq!(found[0].session_file, file);
    std::fs::write(root.path().join("settings.json"), json!({"sessionDir": "relative"}).to_string()).unwrap();
    assert_eq!(sessions_root(root.path(), None), root.path().join("relative"));
    assert_eq!(sessions_root(root.path(), Some(Path::new("/x"))), PathBuf::from("/x"));
}

#[test]
fn session_map_round_trips_and_tolerates_bad_files() {
    let root = tempfile::tempdir().unwrap();
    let store = SessionStore::new(root.path().join("pi-acp/session-map.json"));
    assert!(store.get("a").is_none());
    store.upsert("a", "/cwd", "/cwd/a.jsonl");
    let stored = store.get("a").unwrap();
    assert_eq!((stored.cwd.as_str(), stored.session_file.as_str()), ("/cwd", "/cwd/a.jsonl"));
    let text = std::fs::read_to_string(store.path()).unwrap();
    let file: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(file["version"], 1);
    assert_eq!(file["sessions"]["a"]["sessionId"], "a");
    assert!(file["sessions"]["a"]["updatedAt"].as_str().unwrap().ends_with('Z'));
    assert!(text.ends_with("}\n"));
    store.delete("a");
    assert!(store.get("a").is_none());
    std::fs::write(store.path(), "{\"version\": 2, \"sessions\": {\"a\": {}}}").unwrap();
    assert!(store.get("a").is_none());
}

#[test]
fn delete_removes_stored_or_discovered_sessions_idempotently() {
    let env = Env::new();
    let agent = env.agent(FakeClient::new(), FakeFactory::returning(FakeBackend::new()));
    let stored = env.path().join("stored.jsonl");
    write_session(&stored, &[header("stored-session", "/tmp/delete-project")]);
    agent.store.upsert("stored-session", "/tmp/delete-project", &stored.display().to_string());
    let discovered = env.agent_dir().join("sessions/--tmp--delete-discovery--/0000.jsonl");
    write_session(&discovered, &[header("pi-discovered-session", "/tmp/delete-discovery")]);
    agent.store.upsert("missing-file-session", "/tmp/x", "/tmp/x/0000_non_existent.jsonl");
    for id in ["stored-session", "pi-discovered-session", "non-existent-session", "missing-file-session"] {
        let request = serde_json::from_value(json!({"sessionId": id})).unwrap();
        assert_eq!(serde_json::to_value(agent.delete_session(request)).unwrap(), json!({}));
        assert!(agent.store.get(id).is_none());
    }
    assert!(!stored.exists() && !discovered.exists());
}

/// A history whose title comes from a saved name, the first user message, or a cleared name.
fn history(root: &Path, state: &str) -> (PathBuf, String) {
    let file = root.join("sessions/history.jsonl");
    let first = json!([{"type": "text", "text": "  Original \n"}, {"type": "image", "data": "ignored"},
        {"type": "text", "text": "🚀".repeat(90)}]);
    let mut entries = vec![
        json!({"type": "session", "version": 3, "id": "restored", "cwd": root}),
        json!({"type": "message", "message": {"role": "user", "content": "  \n"}}),
        json!({"type": "message", "message": {"role": "user", "content": [{"type": "image", "data": "ignored"}]}}),
        json!({"type": "message", "message": {"role": "user", "content": first}}),
    ];
    if state != "fallback" {
        entries.push(json!({"type": "session_info", "name": "Saved title"}));
    }
    if state == "cleared" {
        entries.push(json!({"type": "session_info", "name": ""}));
    }
    let assistant = json!({"type": "message", "timestamp": "2026-01-01T00:00:00.000Z",
        "message": {"role": "assistant", "content": "x".repeat(200)}});
    entries.extend(std::iter::repeat_n(assistant, 2100));
    if state == "cleared-in-tail" {
        entries.push(json!({"type": "session_info", "name": ""}));
    }
    write_session(&file, &entries);
    (file, std::fs::read_to_string(root.join("sessions/history.jsonl")).unwrap())
}

#[tokio::test]
async fn restored_sessions_publish_their_title_without_renaming() {
    for mode in ["load", "prompt"] {
        for state in ["saved", "fallback", "cleared", "cleared-in-tail"] {
            let env = Env::new();
            let (file, original) = history(env.path(), state);
            let expected =
                if state == "saved" { "Saved title".to_string() } else { format!("Original {}", "🚀".repeat(71)) };
            let config = FakeConfig {
                messages: json!({"messages": [{"role": "user", "content": "Later"}]}),
                ..FakeConfig::default()
            };
            let (client, backend) = (FakeClient::new(), FakeBackend::with(config));
            let agent = env.agent(client.clone(), FakeFactory::returning(backend.clone()));
            let cwd = env.path().display().to_string();
            agent.store.upsert("restored", &cwd, &file.display().to_string());
            if mode == "load" {
                let request =
                    serde_json::from_value(json!({"sessionId": "restored", "cwd": cwd, "mcpServers": []})).unwrap();
                agent.load_session(request).await.unwrap();
            } else {
                let agent = Arc::clone(&agent);
                let request = serde_json::from_value(
                    json!({"sessionId": "restored", "prompt": [{"type": "text", "text": "Continue"}]}),
                )
                .unwrap();
                let task = tokio::spawn(async move { agent.prompt(request).await });
                settle().await;
                backend.emit(json!({"type": "agent_settled"}));
                task.await.unwrap().unwrap();
            }
            let titles: Vec<Value> =
                client.update_bodies().into_iter().filter(|update| update.get("title").is_some()).collect();
            assert_eq!(titles, [json!({"sessionUpdate": "session_info_update", "title": expected})], "{mode} {state}");
            let listed = list_pi_sessions(&env.path().join("sessions"));
            assert_eq!(listed[0].title.as_deref(), Some(expected.as_str()), "{mode} {state}");
            assert!(backend.calls().is_empty(), "{mode} {state}");
            assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
        }
    }
}
