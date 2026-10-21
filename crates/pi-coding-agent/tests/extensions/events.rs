//! Extension events with their upstream payloads and return semantics.

use pi_ai::providers::faux::FauxResponse;
use pi_ai::{Message, StopReason, UserContent};
use pi_coding_agent::messages::CustomMessage;
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

fn custom_message(custom_type: &str, text: &str) -> CustomMessage {
    CustomMessage {
        custom_type: custom_type.into(),
        content: UserContent::Text(text.into()),
        display: true,
        details: None,
        timestamp: 0,
    }
}

/// Extension that holds `before_agent_start` of the `first` custom message until `/release`.
const HOLD_FIRST: &str = r#"
	let release = () => {};
	pi.on("before_agent_start", async (event) => {
		if (event.messageType !== "custom" || event.message.customType !== "first") return;
		pi.appendEntry("handler-started", {});
		await new Promise((resolve) => { release = resolve; });
	});
	pi.registerCommand("release", { handler: async () => { release(); } });
"#;

fn user_texts(request: &pi_ai::Context) -> Vec<String> {
    request
        .messages
        .iter()
        .filter(|message| message.role() == "user")
        .map(|message| text_of(&serde_json::to_value(message).unwrap()))
        .collect()
}

// Upstream #10267 and #5581: a run started by sendMessage with triggerTurn skipped
// before_agent_start, so its second request patched away the sections handlers had added.
#[tokio::test(flavor = "multi_thread")]
async fn custom_message_runs_emit_before_agent_start_and_keep_one_system_prompt() {
    let body = r#"
	pi.on("before_agent_start", (event) => {
		pi.appendEntry("start", { messageType: event.messageType, customType: event.message?.customType ?? null });
		event.systemPromptOptions.sections.extension_rules = "Answer in one word.";
	});
"#;
    let h = harness(Setup {
        files: vec![("custom-start.ts", with_probe(body))],
        load: vec!["custom-start.ts"],
        responses: vec![
            FauxResponse::text("hi"),
            FauxResponse::tool_call("c1", "probe", json!({"text": "a"})),
            FauxResponse::text("done"),
        ],
        ..Setup::default()
    })
    .await;
    h.session.prompt("hello", Vec::new()).await.unwrap();
    h.session.send_custom_message(custom_message("task-done", "background task finished"), true).await.unwrap();

    let prompts: Vec<String> = h
        .provider
        .requests()
        .iter()
        .map(|request| pi_ai::transcript::flatten_context(request).system_prompt.unwrap_or_default())
        .collect();
    assert_eq!(prompts.len(), 3);
    assert!(prompts[0].contains("Answer in one word."), "{}", prompts[0]);
    assert!(prompts.iter().all(|prompt| *prompt == prompts[0]), "{prompts:?}");
    let starts: Vec<Value> = h
        .entries()
        .into_iter()
        .filter(|entry| entry["customType"] == "start")
        .map(|entry| entry["data"].clone())
        .collect();
    assert_eq!(
        starts,
        [
            json!({"messageType": "user", "customType": null}),
            json!({"messageType": "custom", "customType": "task-done"})
        ]
    );
    assert_eq!(h.errors(), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_custom_message_sent_while_before_agent_start_runs_joins_the_starting_run() {
    let h = harness(Setup {
        files: vec![("hold.ts", with_probe(HOLD_FIRST))],
        load: vec!["hold.ts"],
        responses: vec![FauxResponse::text("handled both")],
        ..Setup::default()
    })
    .await;
    let session = h.session.clone();
    let first =
        tokio::spawn(
            async move { session.send_custom_message(custom_message("first", "task one finished"), true).await },
        );
    eventually(|| h.entries().iter().any(|entry| entry["customType"] == "handler-started")).await;
    assert!(!h.session.is_idle());
    h.session.send_custom_message(custom_message("second", "task two finished"), true).await.unwrap();
    h.session.prompt("/release", Vec::new()).await.unwrap();
    first.await.unwrap().unwrap();

    let requests = h.provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(user_texts(&requests[0]), ["task one finished", "task two finished"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_sent_without_trigger_while_the_run_starts_reaches_its_first_request() {
    let body = r#"
	pi.on("before_agent_start", () => {
		pi.sendMessage({ customType: "note", content: "context from handler", display: true }, { triggerTurn: false });
	});
"#;
    let h = harness(Setup {
        files: vec![("note.ts", with_probe(body))],
        load: vec!["note.ts"],
        responses: vec![FauxResponse::text("done")],
        ..Setup::default()
    })
    .await;
    h.session.prompt("hello", Vec::new()).await.unwrap();
    let requests = h.provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(user_texts(&requests[0]), ["context from handler", "hello"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_abort_requested_while_before_agent_start_runs_aborts_the_run() {
    let h = harness(Setup {
        files: vec![("hold.ts", with_probe(HOLD_FIRST))],
        load: vec!["hold.ts"],
        responses: vec![FauxResponse::text("should not complete")],
        ..Setup::default()
    })
    .await;
    let session = h.session.clone();
    let run = tokio::spawn(async move {
        session.send_custom_message(custom_message("first", "background task finished"), true).await
    });
    eventually(|| h.entries().iter().any(|entry| entry["customType"] == "handler-started")).await;
    let session = h.session.clone();
    let abort = tokio::spawn(async move { session.abort().await });
    h.session.prompt("/release", Vec::new()).await.unwrap();
    run.await.unwrap().unwrap();
    abort.await.unwrap();

    let last = h.session.messages().pop().expect("messages");
    assert!(matches!(&last, Message::Assistant(message) if message.stop_reason == StopReason::Aborted), "{last:?}");
    assert!(h.session.is_idle());
}
