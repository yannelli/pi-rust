//! More upstream example extensions, unchanged: plan mode, todo with renderers, tool override,
//! and session naming.

use pi_ai::providers::faux::FauxResponse;
use pi_coding_agent::extensions::ToolRenderRequest;
use pi_tui::Component;
use serde_json::json;

use crate::harness::{Setup, eventually, example, harness};

/// `text` without ANSI escape sequences.
fn plain(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn plan_mode_plans_read_only_then_executes_and_tracks_steps() {
    let h = harness(Setup {
        files: vec![
            ("plan-mode/index.ts", example("plan-mode/index.ts")),
            ("plan-mode/utils.ts", example("plan-mode/utils.ts")),
        ],
        load: vec!["plan-mode/index.ts"],
        responses: vec![
            FauxResponse::tool_call("c1", "bash", json!({"command": "rm -rf build"})),
            FauxResponse::text("Plan:\n1. Read the configuration file\n2. Update the build script"),
            // Each follow-up runs its own turn: the plan steps, then the execute message.
            FauxResponse::text("Noted the steps."),
            FauxResponse::text("Finished both. [DONE:1] [DONE:2]"),
        ],
        ..Setup::default()
    })
    .await;
    h.session.prompt("/plan", Vec::new()).await.unwrap();
    let tools = h.session.active_tool_names();
    assert!(!tools.contains(&"write".to_string()) && !tools.contains(&"edit".to_string()), "{tools:?}");
    h.ui.answers.lock().unwrap().push("Execute the plan (track progress)".into());
    h.session.prompt("How should we fix the build?", Vec::new()).await.unwrap();
    eventually(|| h.provider.requests().len() == 4).await;
    h.session.wait_for_idle().await;
    eventually(|| h.entries().iter().any(|entry| entry["customType"] == "plan-complete")).await;

    let transcript = h.transcript();
    let blocked = transcript.iter().find(|line| line.starts_with("toolResult: ")).expect("bash result");
    assert!(blocked.contains("Plan mode: command blocked"), "{transcript:?}");
    let calls: Vec<String> = h.ui.calls().iter().map(|call| plain(call)).collect();
    assert!(
        calls.contains(&"notify(info): Plan mode enabled. Built-in write tools disabled.".to_string()),
        "{calls:?}"
    );
    assert!(calls.contains(&"status: plan-mode=⏸ plan".to_string()), "{calls:?}");
    assert!(calls.iter().any(|call| call.starts_with("select: Plan mode - what next? [Execute the plan")), "{calls:?}");
    assert!(calls.iter().any(|call| call.starts_with("widget(aboveEditor): plan-todos=☐ Configuration file")));
    assert!(h.session.active_tool_names().contains(&"write".to_string()), "execution restores the tools");
    let first = h.provider.requests()[0].clone();
    let context: Vec<String> = first.messages.iter().map(|message| serde_json::to_string(message).unwrap()).collect();
    assert!(context.iter().any(|message| message.contains("[PLAN MODE ACTIVE]")));
}

#[tokio::test(flavor = "multi_thread")]
async fn todo_keeps_state_in_tool_results_and_renders_them() {
    let h = harness(Setup {
        files: vec![("todo.ts", example("todo.ts"))],
        load: vec!["todo.ts"],
        from_settings: true,
        responses: vec![
            FauxResponse::tool_call("c1", "todo", json!({"action": "add", "text": "write tests"})),
            FauxResponse::text("added"),
            FauxResponse::tool_call("c2", "todo", json!({"action": "list"})),
            FauxResponse::text("listed"),
        ],
        ..Setup::default()
    })
    .await;
    h.session.prompt("add a todo", Vec::new()).await.unwrap();
    h.session.reload_extensions().await.unwrap();
    h.session.prompt("list them", Vec::new()).await.unwrap();
    let transcript = h.transcript();
    assert!(
        transcript.contains(&"toolResult: [ ] #1: write tests".to_string()),
        "state survives reload: {transcript:?}"
    );

    let runner = h.session.extension_runner().expect("reloaded runner");
    let info = runner.tool_renderer_info("todo").expect("todo renderers");
    assert!(info.call && info.result && !info.self_shell);
    let request = ToolRenderRequest {
        tool_call_id: "c1".into(),
        args: json!({"action": "add", "text": "write tests"}),
        ..ToolRenderRequest::default()
    };
    let result = json!({"content": [{"type": "text", "text": "Added todo #1: write tests"}],
        "details": {"action": "add", "todos": [{"id": 1, "text": "write tests", "done": false}], "nextId": 2}});
    let lines = tokio::task::spawn_blocking(move || {
        let mut call = runner.render_tool_call("todo", &request).expect("call component");
        let mut done = runner.render_tool_result("todo", &result, &request).expect("result component");
        [call.render(60), done.render(60)].concat()
    })
    .await
    .unwrap();
    let lines: Vec<String> = lines.iter().map(|line| plain(line).trim_end().to_string()).collect();
    assert_eq!(lines, vec!["todo add \"write tests\"", "✓ Added #1 write tests"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_override_replaces_the_builtin_read_tool() {
    let h = harness(Setup {
        files: vec![("tool-override.ts", example("tool-override.ts"))],
        load: vec!["tool-override.ts"],
        responses: vec![
            FauxResponse::tool_call("c1", "read", json!({"path": ".env"})),
            FauxResponse::tool_call("c2", "read", json!({"path": "notes.txt"})),
            FauxResponse::text("done"),
        ],
        ..Setup::default()
    })
    .await;
    std::fs::write(h.cwd.join("notes.txt"), "line one\nline two").unwrap();
    h.session.prompt("read files", Vec::new()).await.unwrap();
    let results: Vec<String> = h.transcript().into_iter().filter(|line| line.starts_with("toolResult: ")).collect();
    assert!(results[0].contains("Access denied: \".env\""), "{results:?}");
    assert_eq!(results[1], "toolResult: line one\nline two");
    let log = std::fs::read_to_string(h.agent_dir.join("read-access.log")).unwrap();
    assert!(log.contains("BLOCKED") && log.contains("ALLOWED"), "{log}");
}

#[tokio::test(flavor = "multi_thread")]
async fn session_name_sets_and_shows_the_name() {
    let h = harness(Setup {
        files: vec![("session-name.ts", example("session-name.ts"))],
        load: vec!["session-name.ts"],
        ..Setup::default()
    })
    .await;
    h.session.prompt("/session-name", Vec::new()).await.unwrap();
    h.session.prompt("/session-name Release prep", Vec::new()).await.unwrap();
    h.session.prompt("/session-name", Vec::new()).await.unwrap();
    assert_eq!(
        h.ui.calls(),
        vec![
            "notify(info): No session name set",
            "notify(info): Session named: Release prep",
            "notify(info): Session: Release prep",
        ]
    );
    assert_eq!(h.session.session_name().as_deref(), Some("Release prep"));
}
