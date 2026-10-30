//! The extension UI subprotocol and extension host hooks (upstream `rpc-extension-ui.md`).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use pi_ai::providers::faux::FauxResponse;
use pi_coding_agent::agent_session::{AgentSession, BashResult};
use pi_coding_agent::extension_ui::{DialogOptions, ExtensionMode, ExtensionUi, NotifyType, WidgetPlacement};
use pi_coding_agent::resources::{SourceInfo, SourceOrigin, SourceScope};
use pi_coding_agent::rpc::{
    ExtensionErrorRecord, RpcCommandSource, RpcExtensionBinding, RpcExtensionHost, RpcModeOptions, RpcSlashCommand,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::harness::Options;
use crate::support::{Rpc, start_with};

#[derive(Default)]
struct Host {
    bindings: Mutex<Vec<RpcExtensionBinding>>,
}

#[async_trait]
impl RpcExtensionHost for Host {
    fn bind(&self, binding: RpcExtensionBinding) {
        self.bindings.lock().unwrap().push(binding);
    }

    fn commands(&self, _session: &AgentSession) -> Vec<RpcSlashCommand> {
        let source_info = SourceInfo {
            path: "/ext/deploy.ts".into(),
            source: "local".into(),
            scope: SourceScope::User,
            origin: SourceOrigin::TopLevel,
            base_dir: None,
        };
        vec![RpcSlashCommand {
            name: "deploy".into(),
            description: None,
            source: RpcCommandSource::Extension,
            source_info,
        }]
    }

    async fn user_bash(&self, _session: &AgentSession, command: &str, _exclude: bool) -> Option<BashResult> {
        (command == "remote").then(|| BashResult {
            output: "from extension".into(),
            exit_code: Some(0),
            ..BashResult::default()
        })
    }
}

async fn start_host(options: Options) -> (Rpc, Arc<Host>) {
    let host = Arc::new(Host::default());
    let rpc_options = RpcModeOptions { extension_host: Some(host.clone()), handle_signals: false };
    let rpc = start_with(options, rpc_options, |_, _| {}).await;
    crate::harness::wait_until(|| !host.bindings.lock().unwrap().is_empty()).await;
    (rpc, host)
}

fn binding(host: &Host) -> RpcExtensionBinding {
    host.bindings.lock().unwrap().last().unwrap().clone()
}

async fn request(rpc: &Rpc, method: &str, count: usize) -> Value {
    for _ in 0..2000 {
        let requests: Vec<Value> =
            rpc.records_of("extension_ui_request").into_iter().filter(|record| record["method"] == method).collect();
        if let Some(record) = requests.into_iter().nth(count - 1) {
            return record;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("no {method} request");
}

fn without_id(mut record: Value) -> Value {
    assert!(record["id"].as_str().is_some_and(|id| id.len() == 36), "{record}");
    record.as_object_mut().unwrap().remove("id");
    record
}

#[tokio::test]
async fn dialogs_round_trip_through_extension_ui_responses() {
    let (mut rpc, host) = start_host(Options::default()).await;
    let ui = binding(&host).ui;
    assert_eq!(binding(&host).mode(), ExtensionMode::Rpc);
    assert!(binding(&host).mode().has_ui());

    let select_ui = ui.clone();
    let options = vec!["Allow".to_string(), "Block".to_string()];
    let select = tokio::spawn(async move {
        select_ui
            .select("Allow dangerous command?", &options, DialogOptions::with_timeout(Duration::from_secs(10)))
            .await
    });
    let record = request(&rpc, "select", 1).await;
    let id = record["id"].as_str().unwrap().to_string();
    let expected = json!({"type": "extension_ui_request", "method": "select", "title": "Allow dangerous command?", "options": ["Allow", "Block"], "timeout": 10000});
    assert_eq!(without_id(record), expected);
    rpc.send(json!({"type": "extension_ui_response", "id": id, "value": "Allow"})).await;
    assert_eq!(select.await.unwrap().as_deref(), Some("Allow"));

    let confirm_ui = ui.clone();
    let confirm = tokio::spawn(async move {
        confirm_ui.confirm("Clear session?", "All messages will be lost.", DialogOptions::default()).await
    });
    let record = request(&rpc, "confirm", 1).await;
    let id = record["id"].as_str().unwrap().to_string();
    assert_eq!(
        without_id(record),
        json!({"type": "extension_ui_request", "method": "confirm", "title": "Clear session?", "message": "All messages will be lost."})
    );
    rpc.send(json!({"type": "extension_ui_response", "id": id, "confirmed": true})).await;
    assert!(confirm.await.unwrap());

    let input_ui = ui.clone();
    let input = tokio::spawn(async move {
        input_ui.input("Enter a value", Some("type something..."), DialogOptions::default()).await
    });
    let record = request(&rpc, "input", 1).await;
    assert_eq!(record["placeholder"], "type something...");
    rpc.send(json!({"type": "extension_ui_response", "id": record["id"], "cancelled": true})).await;
    assert_eq!(input.await.unwrap(), None);

    let editor_ui = ui.clone();
    let editor = tokio::spawn(async move {
        editor_ui.editor("Edit some text", Some("Line 1\nLine 2"), DialogOptions::default()).await
    });
    let record = request(&rpc, "editor", 1).await;
    assert_eq!(
        without_id(record.clone()),
        json!({"type": "extension_ui_request", "method": "editor", "title": "Edit some text", "prefill": "Line 1\nLine 2"})
    );
    rpc.send(json!({"type": "extension_ui_response", "id": record["id"], "value": "Edited"})).await;
    assert_eq!(editor.await.unwrap().as_deref(), Some("Edited"));

    let confirm_ui = ui.clone();
    let cancelled = tokio::spawn(async move { confirm_ui.confirm("Sure?", "Really?", DialogOptions::default()).await });
    let record = request(&rpc, "confirm", 2).await;
    rpc.send(json!({"type": "extension_ui_response", "id": record["id"], "cancelled": true})).await;
    assert!(!cancelled.await.unwrap());
    assert_eq!(ui.pending_count(), 0);
    assert!(rpc.records_of("response").is_empty(), "UI responses get no command response");
    rpc.finish().await;
}

#[tokio::test]
async fn dialogs_resolve_to_defaults_on_timeout_and_cancellation() {
    let (mut rpc, host) = start_host(Options::default()).await;
    let ui = binding(&host).ui;
    let answer = ui.select("Pick", &["a".into()], DialogOptions::with_timeout(Duration::from_millis(50))).await;
    assert_eq!(answer, None);
    assert_eq!(request(&rpc, "select", 1).await["timeout"], 50);
    assert!(!ui.confirm("Sure?", "x", DialogOptions::with_timeout(Duration::from_millis(20))).await);
    assert_eq!(ui.pending_count(), 0);
    let late = request(&rpc, "select", 1).await["id"].clone();
    rpc.send(json!({"type": "extension_ui_response", "id": late, "value": "a"})).await;

    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        trigger.cancel();
    });
    let options = DialogOptions { cancel: Some(cancel.clone()), ..DialogOptions::default() };
    assert_eq!(ui.input("Name", None, options.clone()).await, None);
    assert_eq!(ui.editor("Text", None, options).await, None, "already cancelled: no request");
    assert_eq!(rpc.records_of("extension_ui_request").iter().filter(|record| record["method"] == "editor").count(), 0);
    assert_eq!(rpc.data(json!({"id": "s", "type": "get_state"})).await["isStreaming"], false);
    rpc.finish().await;
}

#[tokio::test]
async fn fire_and_forget_requests_and_extension_errors() {
    let (mut rpc, host) = start_host(Options::default()).await;
    let binding = binding(&host);
    let ui = binding.ui.clone();
    ui.notify("Command blocked by user", Some(NotifyType::Warning));
    ui.notify("Plain", None);
    ui.set_status("my-ext", Some("Turn 3 running..."));
    ui.set_status("my-ext", None);
    ui.set_widget(
        "my-ext",
        Some(&["--- My Widget ---".to_string(), "Line 1".to_string()]),
        Some(WidgetPlacement::BelowEditor),
    );
    ui.set_widget("my-ext", None, None);
    ui.set_title("pi - my project");
    ui.set_editor_text("prefilled text");
    ui.paste_to_editor("pasted");
    ui.set_working_message(Some("ignored"));
    assert_eq!(ui.get_editor_text(), "");
    assert!(!ui.get_tools_expanded() && ui.get_all_themes().is_empty());
    assert_eq!(ui.set_theme("dark").unwrap_err(), "Theme switching not supported in RPC mode");
    binding.report_error(&ExtensionErrorRecord {
        extension_path: "/ext/a.ts".into(),
        event: "tool_call".into(),
        error: "boom".into(),
    });

    let error = rpc.wait_for(|record| record["type"] == "extension_error").await;
    assert_eq!(
        error,
        json!({"type": "extension_error", "extensionPath": "/ext/a.ts", "event": "tool_call", "error": "boom"})
    );
    let requests: Vec<Value> = rpc.records_of("extension_ui_request").into_iter().map(without_id).collect();
    let expected = [
        json!({"type": "extension_ui_request", "method": "notify", "message": "Command blocked by user", "notifyType": "warning"}),
        json!({"type": "extension_ui_request", "method": "notify", "message": "Plain"}),
        json!({"type": "extension_ui_request", "method": "setStatus", "statusKey": "my-ext", "statusText": "Turn 3 running..."}),
        json!({"type": "extension_ui_request", "method": "setStatus", "statusKey": "my-ext"}),
        json!({"type": "extension_ui_request", "method": "setWidget", "widgetKey": "my-ext", "widgetLines": ["--- My Widget ---", "Line 1"], "widgetPlacement": "belowEditor"}),
        json!({"type": "extension_ui_request", "method": "setWidget", "widgetKey": "my-ext"}),
        json!({"type": "extension_ui_request", "method": "setTitle", "title": "pi - my project"}),
        json!({"type": "extension_ui_request", "method": "set_editor_text", "text": "prefilled text"}),
        json!({"type": "extension_ui_request", "method": "set_editor_text", "text": "pasted"}),
    ];
    assert_eq!(requests, expected);
    rpc.finish().await;
}

#[tokio::test]
async fn host_commands_bash_and_rebinding() {
    let (mut rpc, host) = start_host(Options::default()).await;
    let commands = rpc.data(json!({"id": "c", "type": "get_commands"})).await["commands"].clone();
    let names: Vec<&str> =
        commands.as_array().unwrap().iter().map(|command| command["name"].as_str().unwrap()).collect();
    assert_eq!(names[..2], ["mcp", "deploy"], "built-in commands, then the host's");
    assert_eq!(
        commands[1],
        json!({"name": "deploy", "source": "extension", "sourceInfo": {"path": "/ext/deploy.ts", "source": "local", "scope": "user", "origin": "top-level"}})
    );

    let remote = rpc.data(json!({"id": "b", "type": "bash", "command": "remote"})).await;
    assert_eq!(remote, json!({"output": "from extension", "exitCode": 0, "cancelled": false, "truncated": false}));
    assert!(rpc.records_of("bash_execution_update").is_empty());
    let messages = rpc.data(json!({"id": "m", "type": "get_messages"})).await["messages"].clone();
    assert_eq!(messages.as_array().unwrap().last().unwrap()["output"], "from extension");

    let first = binding(&host).session.session_id();
    rpc.data(json!({"id": "n", "type": "new_session"})).await;
    let bindings = host.bindings.lock().unwrap().len();
    assert_eq!(bindings, 2);
    assert_ne!(binding(&host).session.session_id(), first);
    rpc.finish().await;
}

#[tokio::test]
async fn extension_shutdown_waits_for_the_command_or_the_run() {
    let (mut rpc, host) = start_host(Options::default()).await;
    binding(&host).shutdown.request();
    assert!(binding(&host).shutdown.is_requested());
    rpc.data(json!({"id": "s", "type": "get_state"})).await;
    assert_eq!(rpc.exit_code().await, 0, "exits after the command");

    let options = Options {
        responses: vec![FauxResponse::text("slow answer")],
        delay: Some(Duration::from_millis(40)),
        ..Options::default()
    };
    let (mut rpc, host) = start_host(options).await;
    rpc.data(json!({"id": "p", "type": "prompt", "message": "Hi"})).await;
    binding(&host).shutdown.request();
    assert_eq!(rpc.exit_code().await, 0, "exits once the run settles");
    assert_eq!(rpc.records_of("agent_settled").len(), 1);
}
