//! `turn_end` and `agent_before_settle` handlers: the context preview they see, the drafts they
//! pass on, and invalid drafts (upstream `emitBoundary` and `_buildBoundaryContext`).

use pi_ai::providers::faux::FauxResponse;
use serde_json::json;

use crate::extensions::harness::{Setup, harness};

const PROBE_TOOL: &str = r#"
	pi.registerTool({
		name: "probe", label: "Probe", description: "Echo", parameters: { type: "object", properties: {} },
		async execute() { return { content: [{ type: "text", text: "echo" }], details: {} }; },
	});
"#;

fn responses() -> Vec<FauxResponse> {
    vec![FauxResponse::tool_call("c1", "probe", json!({})), FauxResponse::text("done")]
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_end_handlers_see_the_context_with_the_session_objects() {
    let body = format!(
        r#"export default function (pi) {{
{PROBE_TOOL}
	const seen = [];
	pi.registerCommand("note", {{ handler: async () => {{
		pi.sendMessage({{ customType: "note", content: "remember", display: true }}, {{ triggerTurn: false }});
	}} }});
	pi.on("turn_end", (event, ctx) => {{
		const context = event.context;
		const last = context.contextEntries.at(-1);
		const note = context.contextMessages.findIndex((message) => message.role === "custom");
		seen.push([
			context === event.context,
			last.sourceEntry === ctx.sessionManager.getEntries().at(-1),
			context.contextMessages.at(-1) === last.sourceEntry.message,
			context.llmMessages.at(-1) === context.contextMessages.at(-1),
			context.contextMessages.map((message) => message.role).join(","),
			context.llmMessages.map((message) => message.role).join(","),
			context.llmMessages[note].content[0].text,
			context.canContinue,
			context.pendingMessages.length,
		].join(" "));
	}});
	pi.registerCommand("seen", {{ handler: async (_args, ctx) => ctx.ui.notify(seen.join(" | ")) }});
}}
"#
    );
    let h = harness(Setup {
        files: vec![("seen.ts", body)],
        load: vec!["seen.ts"],
        responses: responses(),
        ..Setup::default()
    })
    .await;
    h.session.prompt("/note", Vec::new()).await.unwrap();
    h.session.prompt("go", Vec::new()).await.unwrap();
    h.session.prompt("/seen", Vec::new()).await.unwrap();
    // As upstream: the session's own objects, and a custom message reaches the model as a user message.
    let first = "true true true true custom,system,user,assistant,toolResult user,system,user,assistant,toolResult remember true 0";
    let roles = "custom,system,user,assistant,toolResult,assistant user,system,user,assistant,toolResult,assistant";
    let second = format!("true true true true {roles} remember false 0");
    assert_eq!(h.ui.calls(), vec![format!("notify(info): {first} | {second}")]);
    assert_eq!(h.errors(), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn drafts_reach_later_handlers_and_invalid_drafts_fail_for_each_handler() {
    let drafter = r#"export default function (pi) {
	let turn = 0;
	pi.on("turn_end", (event) => {
		turn += 1;
		const draft = turn === 1
			? { type: "custom_message", customType: "boundary-note", content: "from drafter", display: true }
			: { type: "context_edit", targetId: "missing", replacement: null };
		return { entries: [...event.entries, draft] };
	});
}
"#;
    let reader = format!(
        r#"export default function (pi) {{
{PROBE_TOOL}
	const seen = [];
	pi.on("turn_end", (event) => {{
		const {{ contextEntries, contextMessages, llmMessages }} = event.context;
		const last = contextMessages.at(-1);
		seen.push(`${{event.entries.length}} ${{last.role}}:${{last.customType ?? "-"}} ${{llmMessages.at(-1).role}} ${{contextEntries.at(-1).sourceEntry.type}}`);
	}});
	pi.registerCommand("seen", {{ handler: async (_args, ctx) => ctx.ui.notify(seen.join(" | ")) }});
}}
"#
    );
    let h = harness(Setup {
        files: vec![("drafter.ts", drafter.to_string()), ("reader.ts", reader)],
        load: vec!["drafter.ts", "reader.ts"],
        responses: responses(),
        ..Setup::default()
    })
    .await;
    h.session.prompt("go", Vec::new()).await.unwrap();
    h.session.prompt("/seen", Vec::new()).await.unwrap();
    // Turn 1: the reader sees the drafted custom message. Turn 2: the drafts do not apply, so the
    // reader sees the last context that applied, and both handlers report them, as upstream.
    let expected = "1 custom:boundary-note user custom_message | 1 assistant:- assistant message";
    assert_eq!(h.ui.calls(), vec![format!("notify(info): {expected}")]);
    let invalid = "turn_end: Invalid boundary entries: Entry missing not found";
    assert_eq!(h.errors(), vec![invalid.to_string(), invalid.to_string()]);
    let notes: Vec<_> = h.entries().into_iter().filter(|entry| entry["customType"] == "boundary-note").collect();
    assert_eq!(notes.len(), 1, "valid drafts are appended once: {notes:?}");
    assert!(!h.entries().iter().any(|entry| entry["type"] == "context_edit"), "invalid drafts are dropped");
}
