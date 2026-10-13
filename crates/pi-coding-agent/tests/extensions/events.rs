//! Extension events with their upstream payloads and return semantics.

use pi_ai::providers::faux::FauxResponse;
use serde_json::{Value, json};

use crate::harness::{Setup, eventually, harness};

/// An extension tool `probe` that echoes its `text` argument.
const PROBE_TOOL: &str = r#"
	pi.registerTool({
		name: "probe", label: "Probe", description: "Echo", parameters: Type.Object({ text: Type.String() }),
		async execute(_id, params) { return { content: [{ type: "text", text: `echo ${params.text}` }], details: {} }; },
	});
"#;

fn with_probe(body: &str) -> String {
    format!("import {{ Type }} from \"typebox\";\nexport default function (pi) {{\n{PROBE_TOOL}\n{body}\n}}\n")
}

fn text_of(value: &Value) -> String {
    match &value["content"] {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks.iter().filter_map(|block| block["text"].as_str()).collect(),
        _ => String::new(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn lifecycle_events_arrive_in_order() {
    let body = r#"
	const seen = [];
	const types = ["session_start", "input", "before_agent_start", "agent_start", "turn_start", "tool_call",
		"tool_execution_start", "tool_execution_end", "tool_result", "turn_end", "agent_end"];
	for (const type of types) pi.on(type, (event) => { seen.push(event.type === "turn_end" ? `turn_end:${event.turnIndex}` : event.type); });
	pi.on("message_end", (event) => { seen.push(`message_end:${event.message.role}`); });
	pi.registerCommand("seen", { handler: async (_args, ctx) => ctx.ui.notify(seen.join(" ")) });
"#;
    let h = harness(Setup {
        files: vec![("events.ts", with_probe(body))],
        load: vec!["events.ts"],
        responses: vec![FauxResponse::tool_call("c1", "probe", json!({"text": "a"})), FauxResponse::text("done")],
        ..Setup::default()
    })
    .await;
    h.session.prompt("go", Vec::new()).await.unwrap();
    h.session.prompt("/seen", Vec::new()).await.unwrap();
    // Upstream runs `tool_call` and `tool_result` inside the tool execution events.
    let expected = "session_start input before_agent_start agent_start turn_start message_end:system \
        message_end:user message_end:assistant tool_execution_start tool_call tool_result tool_execution_end \
        message_end:toolResult turn_end:0 turn_start message_end:assistant turn_end:1 agent_end";
    assert_eq!(h.ui.calls(), vec![format!("notify(info): {expected}")]);
    assert_eq!(h.errors(), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn before_agent_start_and_context_change_what_the_model_sees() {
    let body = r#"
	pi.on("before_agent_start", (event) => ({
		systemPrompt: `${event.systemPrompt}\nEXTRA RULES`,
		message: { customType: "hint", content: `hint for ${event.prompt}`, display: false },
	}));
	pi.on("context", (event) => ({
		messages: event.messages.map((message) =>
			message.role === "user" && typeof message.content === "string" ? { ...message, content: message.content.toUpperCase() } : message),
	}));
"#;
    let h = harness(Setup {
        files: vec![("context.ts", with_probe(body))],
        load: vec!["context.ts"],
        responses: vec![FauxResponse::text("ok")],
        ..Setup::default()
    })
    .await;
    h.session.prompt("question", Vec::new()).await.unwrap();
    let request = h.provider.requests().pop().expect("request");
    let system: Vec<String> = request
        .messages
        .iter()
        .filter(|message| message.role() == "system")
        .map(|message| text_of(&serde_json::to_value(message).unwrap()))
        .chain(request.system_prompt.clone())
        .collect();
    assert!(system.iter().any(|text| text.ends_with("\nEXTRA RULES")), "{system:?}");
    let texts: Vec<String> = request.messages.iter().map(|m| text_of(&serde_json::to_value(m).unwrap())).collect();
    assert!(texts.contains(&"hint for question".to_string()), "{texts:?}");
    let transcript = h.transcript();
    assert!(
        transcript.contains(&"user: question".to_string()),
        "context edits stay out of the session: {transcript:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_calls_can_be_blocked_and_results_rewritten() {
    let body = r#"
	pi.on("tool_call", (event) => event.input.text === "secret" ? { block: true, reason: "not allowed" } : undefined);
	pi.on("tool_result", (event) => event.isError ? undefined : { content: [{ type: "text", text: `${event.content[0].text} (checked)` }] });
"#;
    let h = harness(Setup {
        files: vec![("tools.ts", with_probe(body))],
        load: vec!["tools.ts"],
        responses: vec![
            FauxResponse::tool_call("c1", "probe", json!({"text": "secret"})),
            FauxResponse::tool_call("c2", "probe", json!({"text": "open"})),
            FauxResponse::text("done"),
        ],
        ..Setup::default()
    })
    .await;
    h.session.prompt("go", Vec::new()).await.unwrap();
    let transcript = h.transcript();
    let results: Vec<&String> = transcript.iter().filter(|line| line.starts_with("toolResult: ")).collect();
    assert_eq!(results.len(), 2, "{transcript:?}");
    assert!(results[0].contains("not allowed"), "{transcript:?}");
    assert_eq!(results[1], "toolResult: echo open (checked)");
}

#[tokio::test(flavor = "multi_thread")]
async fn message_end_replaces_messages_of_the_same_role() {
    let body = r#"
	pi.on("message_end", (event) => {
		if (event.message.role !== "assistant") return undefined;
		return { message: { ...event.message, content: [{ type: "text", text: "rewritten" }] } };
	});
"#;
    let h = harness(Setup {
        files: vec![("rewrite.ts", with_probe(body))],
        load: vec!["rewrite.ts"],
        responses: vec![FauxResponse::text("original")],
        ..Setup::default()
    })
    .await;
    h.session.prompt("go", Vec::new()).await.unwrap();
    assert_eq!(h.transcript(), vec!["system: ", "user: go", "assistant: rewritten"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn compaction_can_be_cancelled_by_extensions() {
    let body = r#"
	pi.on("session_before_compact", (event) => ({ cancel: event.customInstructions === "keep" }));
	pi.on("session_compact", (event) => pi.appendEntry("compacted", { fromExtension: event.fromExtension }));
"#;
    let h = harness(Setup {
        files: vec![("compact.ts", with_probe(body))],
        load: vec!["compact.ts"],
        responses: ["first", "second", "summary", "prefix summary"].map(FauxResponse::text).to_vec(),
        settings: json!({"compaction": {"keepRecentTokens": 1}}),
        ..Setup::default()
    })
    .await;
    h.session.prompt("one", Vec::new()).await.unwrap();
    h.session.prompt("two", Vec::new()).await.unwrap();
    let cancelled = h.session.compact(Some("keep")).await;
    assert!(cancelled.is_err(), "a handler cancelled compaction");
    assert!(h.entries().iter().all(|entry| entry["type"] != "compaction"));
    h.session.compact(None).await.unwrap();
    let entries = h.entries();
    assert!(entries.iter().any(|entry| entry["type"] == "compaction"));
    let compacted = entries.iter().find(|entry| entry["customType"] == "compacted").expect("session_compact");
    assert_eq!(compacted["data"]["fromExtension"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn model_and_thinking_changes_report_their_source() {
    let body = r#"
	pi.on("model_select", (event) => pi.appendEntry("model", { id: event.model.id, previous: event.previousModel?.id, source: event.source }));
	pi.on("thinking_level_select", (event) => pi.appendEntry("thinking", { level: event.level, previous: event.previousLevel }));
"#;
    let h = harness(Setup {
        files: vec![("select.ts", with_probe(body))],
        load: vec!["select.ts"],
        settings: json!({"defaultThinkingLevel": "low"}),
        ..Setup::default()
    })
    .await;
    h.session.set_thinking_level(pi_ai::ThinkingLevel::High, false);
    let other = h.session.model_runtime().registry().find("local", "other").cloned().expect("model");
    h.session.set_model(other, false).await.unwrap();
    eventually(|| h.entries().iter().any(|entry| entry["customType"] == "thinking")).await;
    let entries = h.entries();
    let data = |kind: &str| -> Vec<Value> {
        entries.iter().filter(|entry| entry["customType"] == kind).map(|entry| entry["data"].clone()).collect()
    };
    assert_eq!(data("model"), vec![json!({"id": "other", "previous": "tiny", "source": "set"})]);
    assert_eq!(data("thinking")[0], json!({"level": "high", "previous": "low"}));
}
