//! Built-in commands RPC mode runs without a model call: their `get_commands` entries, the
//! `command_output` record, and `agentInvoked` in `prompt` responses.

use pi_ai::providers::faux::FauxResponse;
use pi_coding_agent::extensions::rpc::ExtensionRpcHost;
use pi_coding_agent::rpc::RpcModeOptions;
use serde_json::json;

use crate::rpc::harness::Options;
use crate::rpc::support::{Rpc, start, start_with};

/// Send `/command` and return its output, asserting a handled response without a run.
async fn run_command(rpc: &mut Rpc, id: &str, message: &str) -> String {
    let data = rpc.data(json!({"id": id, "type": "prompt", "message": message})).await;
    assert_eq!(data, json!({"disposition": "handled", "agentInvoked": false}), "{message}");
    let outputs = rpc.records_of("command_output");
    outputs.last().expect("command_output before the response")["text"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn lists_and_runs_the_builtin_commands_without_a_model_call() {
    let mut rpc = start(Options::default()).await;
    let commands = rpc.data(json!({"id": "c", "type": "get_commands"})).await["commands"].clone();
    for name in ["usage", "context", "extensions", "fast", "reload", "session", "changelog", "export", "name"] {
        let command = commands.as_array().unwrap().iter().find(|command| command["name"] == name);
        let command = command.unwrap_or_else(|| panic!("/{name} listed in {commands}"));
        assert_eq!(command["source"], "extension");
        assert_eq!(command["sourceInfo"]["path"], format!("builtin:{name}"));
        assert_eq!(command["sourceInfo"]["source"], "builtin");
    }

    let fast = run_command(&mut rpc, "f", "/fast on").await;
    assert_eq!(fast, "Fast mode on (the current model has no fast mode)");
    assert!(rpc.h.settings.fast_mode(), "/fast saves fastMode");
    assert_eq!(run_command(&mut rpc, "f2", "/fast").await, "Fast mode off");
    assert_eq!(run_command(&mut rpc, "f3", "/fast later").await, "Unknown fast mode \"later\". Use on or off.");

    let list = run_command(&mut rpc, "e", "/extensions list").await;
    assert!(list.starts_with("```text\nExtensions ([x] enabled, [ ] disabled):"), "{list}");
    assert!(list.contains("[x] builtin:mcp"), "{list}");
    let status = run_command(&mut rpc, "s", "/extensions status").await;
    assert!(status.contains("Built-in extensions ([x] on, [ ] off):") && status.contains("[x] mcp"), "{status}");

    let usage = run_command(&mut rpc, "u", "/usage").await;
    assert!(usage.contains("No login with a usage report"), "{usage}");
    let context = run_command(&mut rpc, "x", "/context").await;
    assert!(context.contains("Model:          local/tiny"), "{context}");
    assert_eq!(run_command(&mut rpc, "n", "/name Release notes").await, "Session name set: Release notes");
    assert!(run_command(&mut rpc, "i", "/session").await.starts_with("Session: "));
    assert_eq!(
        run_command(&mut rpc, "x2", "/export").await,
        "Nothing to export yet (no session messages). Send a prompt first."
    );

    assert!(rpc.records_of("agent_start").is_empty());
    assert!(rpc.h.requests().is_empty(), "no model call");
    rpc.finish().await;
}

const EXTENSION: &str = r#"export default function (pi) {
	pi.registerCommand("ping", {
		description: "Answer with pong",
		handler: async (_args, ctx) => { ctx.ui.notify("pong", "info"); },
	});
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn extension_commands_still_run_after_a_reload() {
    let options = Options { responses: vec![FauxResponse::text("hi")], ..Options::default() };
    let rpc_options = RpcModeOptions { extension_host: Some(ExtensionRpcHost::shared()), handle_signals: false };
    let mut rpc = start_with(options, rpc_options, |h, app| {
        let path = h.cwd.join("ping.ts");
        std::fs::write(&path, EXTENSION).unwrap();
        app.resources.extension_sources = vec![path.display().to_string()];
    })
    .await;
    let ping = |rpc: &Rpc, count: usize| {
        rpc.records().into_iter().filter(|record| record["method"] == "notify" && record["message"] == "pong").count()
            == count
    };

    let data = rpc.data(json!({"id": "p1", "type": "prompt", "message": "/ping"})).await;
    assert_eq!(data, json!({"disposition": "handled", "agentInvoked": false}));
    rpc.wait_for(|_| ping(&rpc, 1)).await;

    let reloaded = run_command(&mut rpc, "r", "/reload").await;
    assert!(reloaded.starts_with("Reloaded extensions, skills, prompts, and context files."), "{reloaded}");
    let commands = rpc.data(json!({"id": "c", "type": "get_commands"})).await["commands"].clone();
    assert!(commands.as_array().unwrap().iter().any(|command| command["name"] == "ping"), "{commands}");
    rpc.data(json!({"id": "p2", "type": "prompt", "message": "/ping"})).await;
    rpc.wait_for(|_| ping(&rpc, 2)).await;

    let started = rpc.data(json!({"id": "p3", "type": "prompt", "message": "hello"})).await;
    assert_eq!(started, json!({"disposition": "started", "agentInvoked": true}));
    rpc.settled(1).await;
    assert!(rpc.records_of("extension_error").is_empty(), "{:?}", rpc.records_of("extension_error"));
    rpc.finish().await;
}
