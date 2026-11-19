//! RPC mode with the extension host: JavaScript extensions list their commands, ask the client
//! through `extension_ui_request` records, and see prompts with source `rpc`.

use pi_ai::providers::faux::FauxResponse;
use pi_coding_agent::extensions::rpc::ExtensionRpcHost;
use pi_coding_agent::rpc::RpcModeOptions;
use serde_json::{Value, json};

use crate::rpc::harness::{Options, text};
use crate::rpc::support::{Rpc, start_with};

const EXTENSION: &str = r#"export default function (pi) {
	pi.on("input", (event, ctx) => { ctx.ui.notify(`source ${event.source}`); });
	pi.registerCommand("greet", {
		description: "Greet someone",
		handler: async (_args, ctx) => {
			const who = await ctx.ui.select("Who?", ["Ada", "Grace"]);
			ctx.ui.notify(`hello ${who}`, "info");
			ctx.ui.setStatus("greet", `greeted ${who}`);
		},
	});
}
"#;

async fn notified(rpc: &Rpc, message: &str) -> Value {
    rpc.wait_for(|record| {
        record["type"] == "extension_ui_request" && record["method"] == "notify" && record["message"] == message
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn extensions_run_with_the_client_as_their_ui() {
    let options = Options { responses: vec![FauxResponse::text("hi there")], ..Options::default() };
    let rpc_options = RpcModeOptions { extension_host: Some(ExtensionRpcHost::shared()), handle_signals: false };
    let mut rpc = start_with(options, rpc_options, |h, _| {
        std::fs::create_dir_all(h.agent_dir.join("extensions")).unwrap();
        std::fs::write(h.agent_dir.join("extensions/greet.ts"), EXTENSION).unwrap();
    })
    .await;
    let commands = rpc.data(json!({"id": "c", "type": "get_commands"})).await["commands"].clone();
    let greet = commands.as_array().unwrap().iter().find(|command| command["name"] == "greet").cloned();
    let greet = greet.expect("extension command listed");
    assert_eq!((greet["source"].as_str(), greet["description"].as_str()), (Some("extension"), Some("Greet someone")));

    rpc.send(json!({"id": "g", "type": "prompt", "message": "/greet"})).await;
    let select = rpc.wait_for(|record| record["method"] == "select").await;
    assert_eq!((select["title"].as_str(), select["options"].clone()), (Some("Who?"), json!(["Ada", "Grace"])));
    rpc.send(json!({"type": "extension_ui_response", "id": select["id"], "value": "Grace"})).await;
    let notice = notified(&rpc, "hello Grace").await;
    assert_eq!(notice["notifyType"], "info");
    let status = rpc.wait_for(|record| record["method"] == "setStatus").await;
    assert_eq!((status["statusKey"].as_str(), status["statusText"].as_str()), (Some("greet"), Some("greeted Grace")));

    rpc.send(json!({"id": "p", "type": "prompt", "message": "hello"})).await;
    notified(&rpc, "source rpc").await;
    rpc.settled(1).await;
    rpc.finish().await;
}

const NOTES: &str = r#"export default function (pi) {
	pi.registerCommand("notes", { handler: async () => {
		pi.sendMessage({ customType: "unset", content: "no display" });
		pi.sendMessage({ customType: "hidden", content: "hidden", display: false });
		pi.sendMessage({ customType: "shown", content: "shown", display: true });
	} });
}
"#;

/// `display` of each custom message in `messages` (`Null` when left out), by `customType`.
fn displays<'a>(messages: impl Iterator<Item = &'a Value>) -> Vec<(String, Option<Value>)> {
    let custom = messages.filter(|message| message.get("customType").is_some());
    custom.map(|message| (message["customType"].as_str().unwrap().into(), message.get("display").cloned())).collect()
}

/// As upstream, a custom message sent without `display` leaves it out of its events, entry, and
/// messages; Paseo hides only `display: false`.
#[tokio::test(flavor = "multi_thread")]
async fn custom_messages_without_display_leave_it_out() {
    let rpc_options = RpcModeOptions { extension_host: Some(ExtensionRpcHost::shared()), handle_signals: false };
    let mut rpc = start_with(Options::default(), rpc_options, |h, _| {
        std::fs::create_dir_all(h.agent_dir.join("extensions")).unwrap();
        std::fs::write(h.agent_dir.join("extensions/notes.ts"), NOTES).unwrap();
    })
    .await;
    rpc.data(json!({"id": "n", "type": "prompt", "message": "/notes"})).await;
    rpc.wait_for(|_| rpc.records_of("message_end").len() == 3).await;
    let expected = vec![
        ("unset".to_string(), None),
        ("hidden".to_string(), Some(json!(false))),
        ("shown".to_string(), Some(json!(true))),
    ];
    let ended = rpc.records_of("message_end");
    assert_eq!(displays(ended.iter().map(|record| &record["message"])), expected);

    let entries = rpc.data(json!({"id": "e", "type": "get_entries"})).await["entries"].clone();
    assert_eq!(displays(entries.as_array().unwrap().iter()), expected);
    let messages = rpc.data(json!({"id": "m", "type": "get_messages"})).await["messages"].clone();
    assert_eq!(displays(messages.as_array().unwrap().iter()), expected);
    rpc.finish().await;
}

const KICK: &str = r#"export default function (pi) {
	// Extension prompts wait here, so the command answers before their run starts.
	pi.on("input", async (event) => {
		if (event.source === "extension") await new Promise((resolve) => setTimeout(resolve, 300));
		return { action: "continue" };
	});
	pi.registerCommand("kick", { handler: async () => { pi.sendUserMessage("from the command"); } });
	pi.registerCommand("noop", { handler: async () => {} });
}
"#;

/// Paseo ends the turn of a prompt with `agentInvoked: false`, so the run an extension command
/// starts with `pi.sendUserMessage` must report true.
#[tokio::test(flavor = "multi_thread")]
async fn a_command_that_sends_a_prompt_reports_that_it_invoked_the_agent() {
    let options = Options { responses: vec![FauxResponse::text("answer")], ..Options::default() };
    let rpc_options = RpcModeOptions { extension_host: Some(ExtensionRpcHost::shared()), handle_signals: false };
    let mut rpc = start_with(options, rpc_options, |h, _| {
        std::fs::create_dir_all(h.agent_dir.join("extensions")).unwrap();
        std::fs::write(h.agent_dir.join("extensions/kick.ts"), KICK).unwrap();
    })
    .await;
    let noop = rpc.data(json!({"id": "n", "type": "prompt", "message": "/noop"})).await;
    assert_eq!(noop, json!({"disposition": "handled", "agentInvoked": false}));

    let kick = rpc.data(json!({"id": "k", "type": "prompt", "message": "/kick"})).await;
    assert_eq!(kick, json!({"disposition": "handled", "agentInvoked": true}));
    assert!(rpc.records_of("agent_start").is_empty(), "the run starts after the response");
    rpc.settled(1).await;
    let requests = rpc.h.requests();
    let prompt = requests.last().and_then(|request| request.messages.iter().rev().find(|m| m.role() == "user"));
    assert_eq!(prompt.map(text).as_deref(), Some("from the command"));
    rpc.finish().await;
}
