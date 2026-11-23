//! RPC mode with the extension host: JavaScript extensions list their commands, ask the client
//! through `extension_ui_request` records, and see prompts with source `rpc`.

use std::path::Path;

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

const DISCOVER: &str = r#"export default function (pi) {
	pi.on("resources_discover", async () => {
		await new Promise((resolve) => setTimeout(resolve, 300));
		return { skillPaths: [SKILLS] };
	});
}
"#;

/// An extension in `agent_dir` whose `resources_discover` adds the skill `late` from `skills` after 300 ms.
pub(crate) fn add_late_skill_extension(agent_dir: &Path, skills: &Path) {
    std::fs::create_dir_all(skills.join("late")).unwrap();
    std::fs::write(skills.join("late/SKILL.md"), "---\nname: late\ndescription: Found late.\n---\nBody").unwrap();
    std::fs::create_dir_all(agent_dir.join("extensions")).unwrap();
    let extension = DISCOVER.replace("SKILLS", &json!(skills).to_string());
    std::fs::write(agent_dir.join("extensions/discover.ts"), extension).unwrap();
}

/// Paseo lists the commands of a new agent as soon as pi starts, before `resources_discover` ends.
#[tokio::test(flavor = "multi_thread")]
async fn get_commands_waits_for_the_skills_extensions_discover() {
    let rpc_options = RpcModeOptions { extension_host: Some(ExtensionRpcHost::shared()), handle_signals: false };
    let mut rpc = start_with(Options::default(), rpc_options, |h, _| {
        add_late_skill_extension(&h.agent_dir, &h.cwd.join("discovered"));
    })
    .await;
    let commands = rpc.data(json!({"id": "c", "type": "get_commands"})).await["commands"].clone();
    let late = commands.as_array().unwrap().iter().find(|command| command["name"] == "skill:late").cloned();
    assert_eq!(late.map(|command| command["source"].clone()), Some(json!("skill")), "{commands}");
    rpc.finish().await;
}

/// A client that sends `get_commands` and closes stdin at once still gets the list, after the
/// extensions bind.
#[tokio::test(flavor = "multi_thread")]
async fn a_query_sent_as_the_input_ends_is_answered() {
    let rpc_options = RpcModeOptions { extension_host: Some(ExtensionRpcHost::shared()), handle_signals: false };
    let mut rpc = start_with(Options::default(), rpc_options, |h, _| {
        add_late_skill_extension(&h.agent_dir, &h.cwd.join("discovered"));
    })
    .await;
    rpc.send(json!({"id": "c", "type": "get_commands"})).await;
    assert_eq!(rpc.finish().await, 0);
    let responses = rpc.responses("c");
    let commands = responses.first().map(|response| response["data"]["commands"].clone());
    let commands = commands.unwrap_or_else(|| panic!("no get_commands response:\n{}", rpc.lines().join("\n")));
    assert!(commands.as_array().unwrap().iter().any(|command| command["name"] == "skill:late"), "{commands}");
}

const TRUST: &str = r#"export default function (pi) {
	pi.on("session_start", async (_event, ctx) => {
		const choice = await ctx.ui.select("Run untrusted hook?", ["Allow once", "Deny"]);
		ctx.ui.notify(`hook ${choice}`);
	});
	pi.registerCommand("hooks", { handler: async () => {} });
}
"#;

/// Paseo probes a new pi with `get_commands` alone and never answers dialogs, and it listens for
/// dialogs only after `get_state`. A `session_start` dialog waits for the prompt.
#[tokio::test(flavor = "multi_thread")]
async fn startup_dialogs_wait_for_a_command_that_acts_on_the_session() {
    let options = Options { responses: vec![FauxResponse::text("done")], ..Options::default() };
    let rpc_options = RpcModeOptions { extension_host: Some(ExtensionRpcHost::shared()), handle_signals: false };
    let mut rpc = start_with(options, rpc_options, |h, _| {
        std::fs::create_dir_all(h.agent_dir.join("extensions")).unwrap();
        std::fs::write(h.agent_dir.join("extensions/trust.ts"), TRUST).unwrap();
    })
    .await;
    rpc.data(json!({"id": "s", "type": "get_state"})).await;
    let commands = rpc.data(json!({"id": "c", "type": "get_commands"})).await["commands"].clone();
    assert!(commands.as_array().unwrap().iter().any(|command| command["name"] == "hooks"), "{commands}");
    assert!(rpc.records_of("extension_ui_request").is_empty(), "held while the client only queries");

    rpc.send(json!({"id": "p", "type": "prompt", "message": "hello"})).await;
    let select = rpc.wait_for(|record| record["method"] == "select").await;
    assert_eq!(select["title"], "Run untrusted hook?");
    assert!(rpc.h.requests().is_empty(), "the prompt waits for session_start");
    rpc.send(json!({"type": "extension_ui_response", "id": select["id"], "value": "Allow once"})).await;
    notified(&rpc, "hook Allow once").await;
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
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(rpc.records_of("prompt_result").is_empty(), "the prompt ran, so no correction");
    rpc.finish().await;
}
