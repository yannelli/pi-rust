//! Prompt turns end with the run they started or joined: a prompt sent while pi runs on its own
//! joins that run, and failures reach the client.

use pi_ai::providers::faux::FauxResponse;
use pi_coding_agent::extensions::rpc::ExtensionRpcHost;
use serde_json::{Value, json};

use crate::acp::{Setup, setup, start, texts};

/// A read-only tool that takes a while, and a background result that starts a run of its own.
const BACKGROUND: &str = r#"export default function (pi) {
	const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
	pi.registerTool({ name: "slow", label: "Slow", description: "Waits", parameters: { type: "object", properties: {} },
		annotations: { readOnlyHint: true },
		async execute() { await wait(1500); return { content: [{ type: "text", text: "slow done" }] }; } });
	pi.on("session_start", () => { setTimeout(() => pi.sendUserMessage("background task finished"), 300); });
}
"#;

/// [`BACKGROUND`], and an input handler that holds the prompt "cancel me" before pi takes it.
const SLOW_INPUT: &str = r#"export default function (pi) {
	const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
	pi.registerTool({ name: "slow", label: "Slow", description: "Waits", parameters: { type: "object", properties: {} },
		annotations: { readOnlyHint: true },
		async execute() { await wait(1500); return { content: [{ type: "text", text: "slow done" }] }; } });
	pi.on("session_start", () => { setTimeout(() => pi.sendUserMessage("background task finished"), 300); });
	pi.on("input", async (event) => { if (event.text === "cancel me") await wait(500); });
}
"#;

fn with_extension(setup: &mut Setup, source: &str) {
    let dir = setup.root.path().join("agent/extensions");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("test.ts"), source).unwrap();
    setup.options.extension_host = Some(ExtensionRpcHost::shared());
}

/// The `agent_message_chunk` text received before the response to request `id`.
fn reply_before_response(seen: &[Value], id: i64) -> String {
    let end = seen.iter().position(|m| m["id"] == id && m.get("method").is_none()).expect("response");
    let updates: Vec<Value> = seen[..end].iter().map(|m| m["params"]["update"].clone()).collect();
    texts(&updates, "agent_message_chunk")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_sent_while_pi_runs_on_its_own_joins_that_run() {
    let mut setup = setup(vec![FauxResponse::tool_call("s1", "slow", json!({})), FauxResponse::text("Both handled.")]);
    with_extension(&mut setup, BACKGROUND);
    let work = setup.root.path().join("work");
    let mut client = start(&setup);
    client.request("initialize", json!({"protocolVersion": 1, "clientCapabilities": {}})).await;
    let created = client.request("session/new", json!({"cwd": work, "mcpServers": []})).await;
    let id = created["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{created}")).to_string();
    client
        .until(|m| m["params"]["update"]["toolCallId"] == "s1" && m["params"]["update"]["status"] == "in_progress")
        .await;

    let prompt = json!({"sessionId": id, "prompt": [{"type": "text", "text": "mid-run question"}]});
    let pending = client.send_request("session/prompt", prompt).await;
    let response = client.response(pending).await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let reply = reply_before_response(&client.seen, pending);
    assert!(reply.ends_with("Both handled."), "the turn stays open until the run it joined settles: {reply}");
    let requests = setup.provider.requests();
    assert_eq!(requests.len(), 2);
    let second = serde_json::to_string(&requests[1].messages).unwrap();
    assert!(second.contains("mid-run question") && second.contains("slow done"), "{second}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_model_error_fails_the_prompt_once_the_retries_are_used_up() {
    let setup = setup(vec![
        FauxResponse::Error("529 overloaded_error: Overloaded".into()),
        FauxResponse::text("Recovered."),
        FauxResponse::Error("400 invalid_request_error: unknown field".into()),
    ]);
    setup.options.app.settings.as_ref().expect("settings").set_retry_base_delay_ms(1);
    let work = setup.root.path().join("work");
    let mut client = start(&setup);
    client.request("initialize", json!({"protocolVersion": 1, "clientCapabilities": {}})).await;
    let created = client.request("session/new", json!({"cwd": work, "mcpServers": []})).await;
    let id = created["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{created}")).to_string();
    let prompt = |text: &str| json!({"sessionId": id, "prompt": [{"type": "text", "text": text}]});

    let retried = client.request("session/prompt", prompt("first")).await;
    assert_eq!(retried["result"]["stopReason"], "end_turn", "{retried}");
    let text = texts(&client.updates(&id), "agent_message_chunk");
    assert!(text.contains("Retrying (attempt 1/") && text.contains("Recovered."), "{text}");

    let failed = client.request("session/prompt", prompt("second")).await;
    assert_eq!(failed["error"], json!({"code": -32603, "message": "400 invalid_request_error: unknown field"}));
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_prompt_that_joined_a_run_drops_it() {
    let mut setup = setup(vec![FauxResponse::tool_call("s1", "slow", json!({})), FauxResponse::text("Fresh answer.")]);
    with_extension(&mut setup, BACKGROUND);
    let work = setup.root.path().join("work");
    let mut client = start(&setup);
    client.request("initialize", json!({"protocolVersion": 1, "clientCapabilities": {}})).await;
    let created = client.request("session/new", json!({"cwd": work, "mcpServers": []})).await;
    let id = created["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{created}")).to_string();
    client
        .until(|m| m["params"]["update"]["toolCallId"] == "s1" && m["params"]["update"]["status"] == "in_progress")
        .await;
    let prompt = |text: &str| json!({"sessionId": id, "prompt": [{"type": "text", "text": text}]});
    let pending = client.send_request("session/prompt", prompt("cancel me")).await;
    client.until(|m| m["params"]["update"]["_meta"]["piAcp"]["running"] == true).await;
    // pi queues the prompt for the run within this time; the slow tool runs on.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    client.notify("session/cancel", json!({"sessionId": id})).await;
    assert_eq!(client.response(pending).await["result"]["stopReason"], "cancelled");

    let fresh = client.request("session/prompt", prompt("fresh question")).await;
    assert_eq!(fresh["result"]["stopReason"], "end_turn", "{fresh}");
    let requests = setup.provider.requests();
    assert_eq!(requests.len(), 2);
    let second = serde_json::to_string(&requests[1].messages).unwrap();
    assert!(second.contains("fresh question") && !second.contains("cancel me"), "{second}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_cancelled_before_pi_took_it_stops_before_its_reply() {
    let mut setup = setup(vec![
        FauxResponse::tool_call("s1", "slow", json!({})),
        FauxResponse::text("Ghost reply."),
        FauxResponse::text("Fresh answer."),
    ]);
    with_extension(&mut setup, SLOW_INPUT);
    let work = setup.root.path().join("work");
    let mut client = start(&setup);
    client.request("initialize", json!({"protocolVersion": 1, "clientCapabilities": {}})).await;
    let created = client.request("session/new", json!({"cwd": work, "mcpServers": []})).await;
    let id = created["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{created}")).to_string();
    client
        .until(|m| m["params"]["update"]["toolCallId"] == "s1" && m["params"]["update"]["status"] == "in_progress")
        .await;
    let prompt = |text: &str| json!({"sessionId": id, "prompt": [{"type": "text", "text": text}]});
    // The input handler holds this prompt; the cancel arrives before pi takes it.
    let pending = client.send_request("session/prompt", prompt("cancel me")).await;
    client.until(|m| m["params"]["update"]["_meta"]["piAcp"]["running"] == true).await;
    client.notify("session/cancel", json!({"sessionId": id})).await;
    assert_eq!(client.response(pending).await["result"]["stopReason"], "cancelled");

    let fresh = client.send_request("session/prompt", prompt("fresh question")).await;
    let response = client.response(fresh).await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let reply = reply_before_response(&client.seen, fresh);
    assert!(reply.ends_with("Fresh answer.") && !reply.contains("Ghost"), "{reply}");
}
