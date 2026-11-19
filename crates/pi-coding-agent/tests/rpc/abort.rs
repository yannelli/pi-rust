//! `abort` with slow extension handlers and tools, prompts sent while a run aborts, and prompts
//! that `agent_settled` handlers send (pi-rust: bounded abort; upstream: settle-time deferral).

use std::time::{Duration, Instant};

use pi_ai::providers::faux::FauxResponse;
use pi_coding_agent::extensions::rpc::ExtensionRpcHost;
use pi_coding_agent::rpc::RpcModeOptions;
use serde_json::{Value, json};

use crate::rpc::harness::Options;
use crate::rpc::support::{Rpc, start_with};

/// A tool that ignores the abort signal, and handlers that each take three seconds.
const SLOW: &str = r#"export default function (pi) {
	const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
	pi.registerTool({ name: "hang", label: "Hang", description: "Waits", parameters: { type: "object", properties: {} },
		async execute() { await wait(3000); return { content: [{ type: "text", text: "late" }] }; } });
	pi.on("tool_result", async () => { await wait(3000); });
	pi.on("tool_execution_end", async () => { await wait(3000); });
	pi.on("turn_end", async () => { await wait(3000); });
	pi.on("agent_end", async () => { await wait(3000); });
	pi.on("agent_settled", async () => { await wait(3000); });
}
"#;

/// `agent_settled` handlers that take three seconds.
const SLOW_SETTLE: &str = r#"export default function (pi) {
	pi.on("agent_settled", async () => { await new Promise((resolve) => setTimeout(resolve, 3000)); });
}
"#;

/// `/later` asks for a prompt; `input` handlers hold extension prompts for 300 ms.
const LATER: &str = r#"export default function (pi) {
	pi.registerCommand("later", { description: "Prompt later", handler: async () => { pi.sendUserMessage("from the extension"); } });
	pi.on("input", async (event) => {
		if (event.source === "extension") await new Promise((resolve) => setTimeout(resolve, 300));
	});
}
"#;

/// The first `agent_settled` handler sends a prompt, then takes a moment to finish.
const SETTLED_PROMPT: &str = r#"export default function (pi) {
	let first = true;
	pi.on("agent_settled", async () => {
		if (!first) return;
		first = false;
		pi.sendUserMessage("again");
		await new Promise((resolve) => setTimeout(resolve, 200));
	});
}
"#;

async fn with_extension(responses: Vec<FauxResponse>, source: &'static str) -> Rpc {
    let options = Options { responses, ..Options::default() };
    let rpc_options = RpcModeOptions { extension_host: Some(ExtensionRpcHost::shared()), handle_signals: false };
    start_with(options, rpc_options, move |h, _| {
        std::fs::create_dir_all(h.agent_dir.join("extensions")).unwrap();
        std::fs::write(h.agent_dir.join("extensions/test.ts"), source).unwrap();
    })
    .await
}

fn position(records: &[Value], predicate: impl Fn(&Value) -> bool) -> usize {
    records.iter().position(predicate).expect("record present")
}

#[tokio::test(flavor = "multi_thread")]
async fn abort_answers_after_the_run_settled_without_waiting_for_slow_handlers() {
    let responses = vec![FauxResponse::tool_call("h1", "hang", json!({})), FauxResponse::text("next")];
    let mut rpc = with_extension(responses, SLOW).await;
    rpc.send(json!({"id": "p", "type": "prompt", "message": "Go"})).await;
    rpc.wait_for(|record| record["type"] == "tool_execution_start").await;
    let started = Instant::now();
    rpc.send(json!({"id": "a", "type": "abort"})).await;
    let reply = rpc.response("a").await;
    // Each slow handler alone takes 3 s; without the grace periods the reply takes 12 s or more.
    // Clients such as Paseo allow 2 s, and CI machines are slow, hence the margin.
    assert!(started.elapsed() < Duration::from_millis(2500), "abort took {:?}", started.elapsed());
    assert_eq!(reply["success"], true);
    let records = rpc.records();
    let settled = position(&records, |record| record == &json!({"type": "agent_settled", "aborted": true}));
    let answered = position(&records, |record| record["id"] == "a");
    assert!(settled < answered, "every event of the aborted run comes before the abort response");
    let result = rpc.records_of("tool_execution_end").pop().expect("tool_execution_end");
    assert_eq!(result["isError"], true);
    // As upstream, a run aborted during tool calls ends with an aborted assistant message.
    let ends = rpc.records_of("message_end");
    let last = ends.iter().rev().find(|end| end["message"]["role"] == "assistant").expect("assistant message");
    assert_eq!(last["message"]["stopReason"], "aborted");
    rpc.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_abort_while_agent_settled_handlers_run_answers_after_the_settled_event() {
    let mut rpc = with_extension(vec![FauxResponse::text("done")], SLOW_SETTLE).await;
    rpc.send(json!({"id": "p", "type": "prompt", "message": "Go"})).await;
    rpc.wait_for(|record| record["type"] == "agent_end").await;
    let started = Instant::now();
    rpc.send(json!({"id": "a", "type": "abort"})).await;
    rpc.response("a").await;
    assert!(started.elapsed() < Duration::from_millis(2500), "abort took {:?}", started.elapsed());
    let records = rpc.records();
    let settled = position(&records, |record| record["type"] == "agent_settled");
    let answered = position(&records, |record| record["id"] == "a");
    assert!(settled < answered, "agent_settled comes before the abort response");
    rpc.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_abort_cancels_an_extension_prompt_that_has_not_started() {
    let mut rpc = with_extension(vec![FauxResponse::text("only for the next prompt")], LATER).await;
    rpc.send(json!({"id": "c", "type": "prompt", "message": "/later"})).await;
    rpc.response("c").await;
    let started = Instant::now();
    rpc.send(json!({"id": "a", "type": "abort"})).await;
    rpc.response("a").await;
    assert!(started.elapsed() < Duration::from_millis(1500), "abort took {:?}", started.elapsed());
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(rpc.records_of("agent_start").is_empty(), "the stopped extension prompt must not run");
    rpc.send(json!({"id": "p", "type": "prompt", "message": "Next"})).await;
    assert_eq!(rpc.response("p").await["success"], true);
    assert_eq!(rpc.settled(1).await, json!({"type": "agent_settled", "aborted": false}));
    rpc.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_sent_while_a_run_aborts_starts_after_it() {
    let responses = vec![FauxResponse::tool_call("h1", "hang", json!({})), FauxResponse::text("second answer")];
    let mut rpc = with_extension(responses, SLOW).await;
    rpc.send(json!({"id": "p1", "type": "prompt", "message": "Go"})).await;
    rpc.wait_for(|record| record["type"] == "tool_execution_start").await;
    rpc.send(json!({"id": "a", "type": "abort"})).await;
    rpc.send(json!({"id": "p2", "type": "prompt", "message": "Again"})).await;
    let reply = rpc.response("p2").await;
    assert_eq!(reply["success"], true, "a prompt during the abort must not fail: {reply}");
    assert_eq!(rpc.settled(2).await, json!({"type": "agent_settled", "aborted": false}));
    let records = rpc.records();
    let aborted = position(&records, |record| record == &json!({"type": "agent_settled", "aborted": true}));
    let restarted = records.iter().enumerate().filter(|(_, record)| record["type"] == "agent_start").nth(1);
    let (restarted, _) = restarted.expect("a second run");
    assert!(aborted < restarted, "the new run starts after the aborted one settled");
    rpc.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_from_an_agent_settled_handler_starts_after_the_run_settled() {
    let responses = vec![FauxResponse::text("one"), FauxResponse::text("two")];
    let mut rpc = with_extension(responses, SETTLED_PROMPT).await;
    rpc.send(json!({"id": "p", "type": "prompt", "message": "Go"})).await;
    rpc.settled(2).await;
    let records = rpc.records();
    let settled = position(&records, |record| record["type"] == "agent_settled");
    let starts: Vec<usize> = (0..records.len()).filter(|&index| records[index]["type"] == "agent_start").collect();
    assert_eq!(starts.len(), 2);
    assert!(settled < starts[1], "upstream defers the prompt until agent_settled was emitted");
    rpc.finish().await;
}
