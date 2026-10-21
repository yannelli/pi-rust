//! `reachesTools` on tools extensions register (upstream `ToolDefinition.reachesTools`): the agent
//! tool carries it, `pi.getAllTools()` reports it, and codemode and MCP treat the tool as reaching
//! the tools it names.

use std::time::Duration;

use pi_agent::ToolReach;
use pi_ai::providers::faux::FauxResponse;
use pi_ai::{ContentBlock, Message, ToolResultMessage};
use serde_json::{Value, json};

use crate::harness::{Harness, Setup, harness};
use crate::mcp_fake::{Fake, FakeOptions};

const TOOLS: &str = r#"import { Type } from "typebox";
const tool = (name, extra = {}) => ({
	name, label: name, description: name, parameters: Type.Object({}),
	async execute() { return { content: [{ type: "text", text: name }] }; },
	...extra,
});
const reachesTools = (input) => (input.all ? "all" : String(input.query ?? "").split(" ").filter(Boolean));
export default function (pi) {
	pi.registerTool(tool("reach", { reachesTools }));
	pi.registerTool(tool("plain"));
	pi.registerTool(tool("throws", { reachesTools: () => { throw new Error("boom"); } }));
	pi.registerCommand("report", { description: "Report", handler: async (_args, ctx) => {
		const find = (name) => pi.getAllTools().find((tool) => tool.name === name);
		ctx.ui.notify(JSON.stringify({
			same: find("reach").reachesTools === reachesTools,
			reach: find("reach").reachesTools({ query: "docs search" }),
			plain: typeof find("plain").reachesTools,
			bash: typeof find("bash").reachesTools,
			codemode: find("codemode").reachesTools({ code: "await tools.mcp__docs__search({})" }),
			codemodeAll: find("codemode").reachesTools({ code: "return ALL_TOOLS.length" }),
			serialized: Object.keys(JSON.parse(JSON.stringify(find("reach")))).includes("reachesTools"),
		}));
	} });
}
"#;

fn words(words: &[&str]) -> Option<ToolReach> {
    Some(ToolReach::Words(words.iter().map(|word| word.to_string()).collect()))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_registered_tool_reaches_what_its_reaches_tools_returns() {
    let h =
        harness(Setup { files: vec![("tools.ts", TOOLS.into())], load: vec!["tools.ts"], ..Setup::default() }).await;
    let tool = |name: &str| h.session.tool(name).unwrap_or_else(|| panic!("no tool {name}"));

    assert_eq!(tool("reach").reaches_tools(&json!({ "all": true })), Some(ToolReach::All));
    assert_eq!(
        tool("reach").reaches_tools(&json!({ "query": "docs mcp__docs__search" })),
        words(&["docs", "mcp__docs__search"])
    );
    assert_eq!(tool("reach").reaches_tools(&json!({})), words(&[]));
    assert_eq!(tool("plain").reaches_tools(&json!({ "all": true })), None);
    assert!(tool("reach").declares_reaches_tools() && !tool("plain").declares_reaches_tools());
    assert!(h.errors().is_empty(), "{:?}", h.errors());

    // A handler that throws reaches nothing, and the error reaches the extension error listeners.
    assert_eq!(tool("throws").reaches_tools(&json!({})), words(&[]));
    assert!(h.errors().iter().any(|error| error.starts_with("reachesTools: ") && error.contains("boom")));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_tool_listing_reports_reaches_tools_as_upstream_does() {
    let h =
        harness(Setup { files: vec![("tools.ts", TOOLS.into())], load: vec!["tools.ts"], ..Setup::default() }).await;
    let reaching: Vec<String> =
        h.session.all_tools().into_iter().filter(|tool| tool.reaches_tools).map(|tool| tool.name).collect();
    assert!(
        ["reach", "throws", "codemode"].iter().all(|name| reaching.iter().any(|tool| tool == name)),
        "{reaching:?}"
    );
    assert!(!reaching.iter().any(|name| name == "plain" || name == "bash"), "{reaching:?}");

    h.session.prompt("/report", Vec::new()).await.unwrap();
    let calls = h.ui.calls();
    let report =
        calls.iter().find_map(|call| call.strip_prefix("notify(info): ")).unwrap_or_else(|| panic!("{calls:?}"));
    let report: Value = serde_json::from_str(report).unwrap();
    assert_eq!(report["same"], true, "an extension's own function");
    assert_eq!(report["reach"], json!(["docs", "search"]));
    assert_eq!(report["plain"], "undefined");
    assert_eq!(report["bash"], "undefined");
    assert_eq!(report["codemodeAll"], "all");
    assert_eq!(report["codemode"], json!(["await", "tools", "mcp__docs__search"]));
    assert_eq!(report["serialized"], false, "a function has no JSON form");
}

fn tool_result(h: &Harness, name: &str) -> ToolResultMessage {
    let mut results = h.session.messages().into_iter().filter_map(|message| match message {
        Message::ToolResult(result) if result.tool_name == name => Some(result),
        _ => None,
    });
    results.next_back().unwrap_or_else(|| panic!("no result of {name}"))
}

fn text(result: &ToolResultMessage) -> String {
    result.content.iter().filter_map(ContentBlock::as_text).collect::<Vec<_>>().join("\n")
}

/// A tool that calls `mcp__docs__search`, which a codemode-exposed server registers once it is up.
fn search_tool(extra: &str) -> String {
    format!(
        r#"import {{ Type }} from "typebox";
export default function (pi) {{
	pi.registerTool({{ name: "reach", label: "reach", description: "reach", parameters: Type.Object({{}}),
		{extra}
		async execute(_id, _params, _signal, _update, ctx) {{
			const result = await ctx.executeTool("mcp__docs__search", {{ query: "q" }});
			return {{ content: [{{ type: "text", text: JSON.stringify(result) }}] }};
		}} }});
}}
"#
    )
}

async fn mcp_harness(source: String, fake: FakeOptions) -> (Harness, Fake) {
    let fake = Fake::new(fake);
    let mcp =
        fake.session_options("docs", json!({ "url": "http://unused.invalid", "exposure": "codemode" }), json!({}));
    let setup = Setup {
        files: vec![("reach.ts", source)],
        load: vec!["reach.ts"],
        responses: vec![FauxResponse::tool_call("c1", "reach", json!({})), FauxResponse::text("done")],
        mcp: Some(mcp),
        ..Setup::default()
    };
    (harness(setup).await, fake)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_waits_for_the_servers_its_reaches_tools_names() {
    let slow = FakeOptions { initialize_delay: Some(Duration::from_millis(30)), ..FakeOptions::default() };
    let (h, fake) = mcp_harness(search_tool(r#"reachesTools: () => ["mcp__docs__search"],"#), slow).await;
    h.session.prompt("go", Vec::new()).await.unwrap();

    let result = tool_result(&h, "reach");
    assert!(!result.is_error, "{}", text(&result));
    assert!(text(&result).contains("q guide"), "{}", text(&result));
    assert_eq!(fake.calls(), [r#"search:{"query":"q"}"#]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_does_not_wait_for_servers_its_reaches_tools_does_not_name() {
    let never = FakeOptions { initialize_delay: Some(Duration::MAX), ..FakeOptions::default() };
    for extra in [r#"reachesTools: () => ["mcp__other__search"],"#, ""] {
        let (h, fake) = mcp_harness(search_tool(extra), never.clone()).await;
        h.session.prompt("go", Vec::new()).await.unwrap();

        let result = tool_result(&h, "reach");
        assert!(text(&result).contains("Tool mcp__docs__search not found"), "{extra}: {}", text(&result));
        assert!(fake.calls().is_empty());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn codemode_replacement_is_activated_for_mcp_only_when_it_reaches_tools() {
    const REPLACEMENT: &str = r#"import { Type } from "typebox";
export default function (pi) {
	pi.registerTool({ name: "codemode", label: "Mine", description: "Replacement", parameters: Type.Object({}),
		defaultActive: false, EXTRA
		async execute() { return { content: [{ type: "text", text: "replaced" }] }; } });
}
"#;
    for (extra, active) in [("reachesTools: () => \"all\",", true), ("", false)] {
        let fake = Fake::new(FakeOptions::default());
        let mcp =
            fake.session_options("docs", json!({ "url": "http://unused.invalid", "exposure": "codemode" }), json!({}));
        let source = REPLACEMENT.replace("EXTRA", extra);
        let setup = Setup {
            files: vec![("codemode.ts", source)],
            load: vec!["codemode.ts"],
            responses: vec![FauxResponse::text("ready")],
            settings: json!({ "defaultTools": [] }),
            mcp: Some(mcp),
            ..Setup::default()
        };
        let h = harness(setup).await;
        h.session.set_active_tools_by_name(&[]);
        h.session.prompt("start", Vec::new()).await.unwrap();

        let expected: &[&str] = if active { &["codemode"] } else { &[] };
        assert_eq!(h.session.active_tool_names(), expected, "{extra}");
    }
}
