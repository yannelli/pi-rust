//! `pi.callMcpTool`: an extension calls a tool of a connected MCP server outside of a tool call.

use std::sync::Arc;
use std::time::Duration;

use pi_coding_agent::extension_ui::ExtensionMode;
use serde_json::{Value, json};

use crate::extensions::harness::{Setup, harness};
use crate::extensions::mcp_fake::{Fake, FakeOptions};
use crate::extensions::native::{NativeUi, entry_data, native_in};

const CALLER: &str = r#"export default function (pi) {
	const seen = [];
	pi.on("tool_call", (event) => { seen.push(event.toolName); });
	const attempt = (...args) => pi.callMcpTool(...args).then((result) => ({ result }), (error) => ({ error: error.message, code: error.code }));
	pi.registerCommand("call", { description: "Call MCP", handler: async (_args, ctx) => {
		const results = {
			search: await attempt("docs", "search", { query: "q" }),
			fail: await attempt("docs", "fail"),
			unknownServer: await attempt("nope", "search", {}),
			unknownTool: await attempt("docs", "nope", {}),
			badArgs: await attempt("docs", "search", [1]),
			seen,
		};
		ctx.ui.notify(JSON.stringify(results));
	} });
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn call_mcp_tool_calls_a_server_tool_without_a_tool_call() {
    let slow = FakeOptions { initialize_delay: Some(Duration::from_millis(30)), ..FakeOptions::default() };
    let fake = Fake::new(slow);
    let mcp =
        fake.session_options("docs", json!({ "url": "http://unused.invalid", "exposure": "codemode" }), json!({}));
    let files = vec![("call.ts", CALLER.to_string())];
    let h = harness(Setup { files, load: vec!["call.ts"], mcp: Some(mcp), ..Setup::default() }).await;
    // The server is still connecting; the call waits for it.
    h.session.prompt("/call", Vec::new()).await.unwrap();

    let calls = h.ui.calls();
    let line = calls.iter().find_map(|call| call.strip_prefix("notify(info): ")).expect("results");
    let results: Value = serde_json::from_str(line).unwrap();
    let search = json!({ "content": [{ "type": "text", "text": "q guide\nq faq" }], "isError": false,
        "structuredContent": { "hits": ["q guide", "q faq"] } });
    assert_eq!(results["search"]["result"], search);
    let fail = json!({ "content": [{ "type": "text", "text": "server exploded" }], "isError": true });
    assert_eq!(results["fail"]["result"], fail);
    assert_eq!(results["unknownServer"], json!({ "error": "Unknown MCP server \"nope\"", "code": -32602 }));
    assert_eq!(results["unknownTool"], json!({ "error": "MCP server \"docs\" has no tool \"nope\"", "code": -32602 }));
    assert_eq!(results["badArgs"]["code"], -32602);
    // Not a model tool call: no tool events and nothing in the session.
    assert_eq!(results["seen"], json!([]));
    assert_eq!(fake.calls(), [r#"search:{"query":"q"}"#, "fail:{}"]);
    assert!(h.session.messages().is_empty(), "{:?}", h.transcript());
}

/// The native fixture with the session MCP server `docs` of `fake`.
async fn native_with_mcp(fake: &Fake) -> crate::extensions::harness::Harness {
    let mcp = fake.session_options("docs", json!({ "url": "http://unused.invalid" }), json!({}));
    let ui = Arc::new(NativeUi::default());
    native_in(Setup { mcp: Some(mcp), ..Setup::default() }, &[], ExtensionMode::Tui, ui, false).await
}

#[tokio::test(flavor = "multi_thread")]
async fn native_mcp_call_tool_answers_results_and_error_codes() {
    let fake = Fake::new(FakeOptions::default());
    let h = native_with_mcp(&fake).await;
    h.session.prompt("/native-mcp", Vec::new()).await.unwrap();

    let data = entry_data(&h, "native-mcp").expect("native-mcp entry");
    let search = json!({ "content": [{ "type": "text", "text": "n guide\nn faq" }], "isError": false,
        "structuredContent": { "hits": ["n guide", "n faq"] } });
    assert_eq!(data["search"], json!({ "result": search }));
    assert_eq!(data["fail"]["result"]["isError"], true);
    assert_eq!(data["unknown"], json!({ "error": "Unknown MCP server \"nope\"", "code": -32602 }));
}

#[tokio::test(flavor = "multi_thread")]
async fn native_mcp_call_tool_times_out_and_cancels_while_the_server_connects() {
    let fake = Fake::new(FakeOptions { initialize_delay: Some(Duration::MAX), ..FakeOptions::default() });
    let h = native_with_mcp(&fake).await;
    h.session.prompt("/native-mcp-wait", Vec::new()).await.unwrap();

    let data = entry_data(&h, "native-mcp-wait").expect("native-mcp-wait entry");
    let timeout = json!({ "error": "MCP server \"docs\" did not connect within 50ms", "code": -32005 });
    assert_eq!(data["timeout"], timeout);
    assert_eq!(data["cancelled"], json!({ "error": "MCP tool call cancelled", "code": -32800 }));
    assert!(fake.calls().is_empty());
}
