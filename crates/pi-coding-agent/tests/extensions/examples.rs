//! Upstream example extensions, unchanged.

use pi_ai::providers::faux::FauxResponse;
use serde_json::json;

use crate::harness::{Setup, example, harness};

#[tokio::test(flavor = "multi_thread")]
async fn hello_registers_a_tool_the_model_calls() {
    let h = harness(Setup {
        files: vec![("hello.ts", example("hello.ts"))],
        load: vec!["hello.ts"],
        responses: vec![FauxResponse::tool_call("call-1", "hello", json!({"name": "Ada"})), FauxResponse::text("done")],
        ..Setup::default()
    })
    .await;
    assert_eq!(h.errors(), Vec::<String>::new());
    assert!(h.session.active_tool_names().contains(&"hello".to_string()), "{:?}", h.session.active_tool_names());
    h.session.prompt("greet", Vec::new()).await.unwrap();
    let transcript = h.transcript();
    assert!(transcript.iter().any(|line| line == "toolResult: Hello, Ada!"), "{transcript:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_lists_commands_in_a_selector() {
    let h = harness(Setup {
        files: vec![("commands.ts", example("commands.ts"))],
        load: vec!["commands.ts"],
        ..Setup::default()
    })
    .await;
    h.ui.answers.lock().unwrap().push("/commands - List available slash commands".into());
    *h.ui.confirm.lock().unwrap() = true;
    let disposition = h.session.prompt("/commands", Vec::new()).await.unwrap();
    assert_eq!(disposition.as_str(), "handled");
    let calls = h.ui.calls();
    // Skills of the user's home directory may follow the extension commands.
    let listing = "select: Available Commands [--- Extensions ---, /commands - List available slash commands";
    assert!(calls[0].starts_with(listing), "{calls:?}");
    assert!(calls[1].starts_with("confirm: commands / View source path?"), "{calls:?}");
    assert!(calls[2].starts_with("notify(info): ") && calls[2].ends_with("commands.ts"), "{calls:?}");
    assert!(h.provider.requests().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn dynamic_tools_register_after_start_and_from_commands() {
    let h = harness(Setup {
        files: vec![("dynamic-tools.ts", example("dynamic-tools.ts"))],
        load: vec!["dynamic-tools.ts"],
        responses: vec![
            FauxResponse::tool_call("c1", "echo_session", json!({"message": "hi"})),
            FauxResponse::text("ok"),
        ],
        ..Setup::default()
    })
    .await;
    assert_eq!(h.ui.calls(), vec!["notify(info): Registered dynamic tool: echo_session"]);
    assert!(h.session.active_tool_names().contains(&"echo_session".to_string()));
    h.session.prompt("/add-echo-tool shout", Vec::new()).await.unwrap();
    assert!(h.session.active_tool_names().contains(&"shout".to_string()));
    h.session.prompt("/add-echo-tool shout", Vec::new()).await.unwrap();
    assert_eq!(h.ui.calls().last().unwrap(), "notify(warning): Tool already registered: shout");
    h.session.prompt("echo", Vec::new()).await.unwrap();
    assert!(h.transcript().contains(&"toolResult: [session] hi".to_string()), "{:?}", h.transcript());
    let prompt = h.session.system_prompt();
    assert!(prompt.contains("Use echo_session when the user asks for exact echo output."), "{prompt}");
}

#[tokio::test(flavor = "multi_thread")]
async fn event_bus_delivers_between_handlers() {
    let h = harness(Setup {
        files: vec![("event-bus.ts", example("event-bus.ts"))],
        load: vec!["event-bus.ts"],
        ..Setup::default()
    })
    .await;
    h.session.prompt("/emit hi there", Vec::new()).await.unwrap();
    assert_eq!(
        h.ui.calls(),
        vec![
            "notify(info): Event from event-bus-example: Session started",
            "notify(info): Event from /emit command: hi there"
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn permission_gate_asks_before_dangerous_commands() {
    let h = harness(Setup {
        files: vec![("permission-gate.ts", example("permission-gate.ts"))],
        load: vec!["permission-gate.ts"],
        responses: vec![
            FauxResponse::tool_call("c1", "bash", json!({"command": "rm -rf build"})),
            FauxResponse::text("stopped"),
        ],
        ..Setup::default()
    })
    .await;
    h.ui.answers.lock().unwrap().push("No".into());
    h.session.prompt("clean", Vec::new()).await.unwrap();
    assert!(h.ui.calls()[0].starts_with("select: ⚠️ Dangerous command:"), "{:?}", h.ui.calls());
    assert!(h.transcript().contains(&"toolResult: Blocked by user".to_string()), "{:?}", h.transcript());
}

#[tokio::test(flavor = "multi_thread")]
async fn permission_gate_blocks_without_a_ui() {
    let h = harness(Setup {
        files: vec![("permission-gate.ts", example("permission-gate.ts"))],
        load: vec!["permission-gate.ts"],
        responses: vec![FauxResponse::tool_call("c1", "bash", json!({"command": "sudo ls"})), FauxResponse::text("no")],
        mode: pi_coding_agent::extension_ui::ExtensionMode::Print,
        ..Setup::default()
    })
    .await;
    h.session.prompt("list", Vec::new()).await.unwrap();
    let expected = "toolResult: Dangerous command blocked (no UI for confirmation)".to_string();
    assert!(h.transcript().contains(&expected), "{:?}", h.transcript());
}

#[tokio::test(flavor = "multi_thread")]
async fn protected_paths_block_writes() {
    let h = harness(Setup {
        files: vec![("protected-paths.ts", example("protected-paths.ts"))],
        load: vec!["protected-paths.ts"],
        responses: vec![
            FauxResponse::tool_call("c1", "write", json!({"path": ".env", "content": "SECRET=1"})),
            FauxResponse::text("done"),
        ],
        ..Setup::default()
    })
    .await;
    h.session.prompt("write env", Vec::new()).await.unwrap();
    assert_eq!(h.ui.calls(), vec!["notify(warning): Blocked write to protected path: .env"]);
    assert!(h.transcript().contains(&"toolResult: Path \".env\" is protected".to_string()), "{:?}", h.transcript());
    assert!(!h.cwd.join(".env").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn input_transform_handles_and_rewrites_input() {
    let h = harness(Setup {
        files: vec![("input-transform.ts", example("input-transform.ts"))],
        load: vec!["input-transform.ts"],
        responses: vec![FauxResponse::text("TypeScript is JavaScript with types.")],
        ..Setup::default()
    })
    .await;
    let handled = h.session.prompt("ping", Vec::new()).await.unwrap();
    assert_eq!(handled.as_str(), "handled");
    assert_eq!(h.ui.calls(), vec!["notify(info): pong"]);
    h.session.prompt("?quick What is TypeScript?", Vec::new()).await.unwrap();
    let transcript = h.transcript();
    assert!(
        transcript.contains(&"user: Respond briefly in 1-2 sentences: What is TypeScript?".to_string()),
        "{transcript:?}"
    );
    assert_eq!(h.provider.requests().len(), 1);
}
