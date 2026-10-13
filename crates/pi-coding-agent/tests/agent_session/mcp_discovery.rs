//! How MCP tools are reached (upstream `agent-session-mcp.test.ts`): the `mcp_servers` prompt
//! section, codemode and tool_search activation, waits for servers, and script discovery.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pi_agent::{AgentTool, AgentToolResult, ToolUpdateCallback};
use pi_ai::Message;
use pi_ai::providers::faux::FauxResponse;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::harness::wait_until;
use crate::mcp_fake::FakeOptions;
use crate::tool_support::{
    SetupOptions, declared_description, declared_tool_names, last_line, result_text, servers_section, setup,
    tool_result,
};

fn instructions(text: &str) -> FakeOptions {
    FakeOptions { instructions: Some(text.into()), ..FakeOptions::default() }
}

fn slow(delay: Duration) -> FakeOptions {
    FakeOptions { initialize_delay: Some(delay), ..FakeOptions::default() }
}

#[tokio::test]
async fn describes_the_server_with_its_description_and_returns_its_instructions_to_scripts() {
    let options = SetupOptions {
        default_tools: vec!["tool_search"],
        fake: instructions("Always search before reading."),
        ..SetupOptions::default()
    };
    let s = setup(json!({ "exposure": "codemode", "description": "Search the product docs" }), options).await;
    let code = r#"const docs = await describeNamespace("mcp__docs");
const aliases = await Promise.all(["docs", "mcp__docs"].map((name) => describeNamespace(name)));
return { docs, same: aliases.every((alias) => JSON.stringify(alias) === JSON.stringify(docs)) };"#;
    s.h.provider.push(FauxResponse::tool_call("c1", "codemode", json!({ "code": code })));
    s.h.provider.push(FauxResponse::text("done"));
    s.h.session.prompt("go", Vec::new()).await.unwrap();

    let session = &s.h.session;
    let codemode = declared_description(session, "codemode").unwrap();
    assert!(!codemode.contains("mcp__docs") && !codemode.contains("Always search"));
    assert_eq!(declared_description(session, "tool_search").unwrap(), pi_codemode::TOOL_SEARCH_DESCRIPTION);
    let returned: Value = serde_json::from_str(&last_line(&tool_result(session, "codemode"))).unwrap();
    assert_eq!(
        returned,
        json!({ "docs": { "name": "mcp__docs", "description": "Search the product docs",
            "instructions": "Always search before reading.",
            "tools": ["mcp__docs__search", "mcp__docs__fail", "mcp__docs__shot"] }, "same": true })
    );
    assert!(servers_section(session).unwrap().contains("- mcp__docs (codemode): Search the product docs"));
}

#[tokio::test]
async fn lists_servers_by_the_first_line_of_their_instructions() {
    let options = SetupOptions { fake: instructions("Docs search.\nLong guidance."), ..SetupOptions::default() };
    let s = setup(json!({ "exposure": "deferred" }), options).await;
    s.h.provider.push(FauxResponse::text("done"));
    s.h.session.prompt("go", Vec::new()).await.unwrap();

    let section = servers_section(&s.h.session).unwrap();
    assert!(section.contains("- mcp__docs (tool_search): Docs search.\n"), "{section}");
    assert!(!section.contains("Long guidance"));
}

#[tokio::test]
async fn does_not_activate_codemode_when_auto_enable_codemode_is_false() {
    let options = SetupOptions { extra: json!({ "autoEnableCodemode": false }), ..SetupOptions::default() };
    let s = setup(json!({ "exposure": "codemode" }), options).await;
    s.h.provider.push(FauxResponse::text("ready"));
    s.h.session.prompt("start", Vec::new()).await.unwrap();

    assert!(s.h.session.active_tool_names().is_empty());
    assert!(s.h.session.callable_tool_names().contains(&"mcp__docs__search".to_string()));
    assert_eq!(
        *s.notifications.lock().unwrap(),
        [
            "MCP tools are only reachable from the codemode or tool_search tool, but neither is active (autoEnableCodemode is false); they cannot be called."
        ]
    );
}

#[tokio::test]
async fn treats_codemode_mcp_tools_as_reachable_through_an_active_tool_search() {
    let options = SetupOptions {
        default_tools: vec!["tool_search"],
        extra: json!({ "autoEnableCodemode": false }),
        ..SetupOptions::default()
    };
    let s = setup(json!({ "exposure": "codemode" }), options).await;
    s.h.provider.push(FauxResponse::tool_call("t1", "tool_search", json!({ "query": "search the docs", "limit": 1 })));
    s.h.provider.push(FauxResponse::tool_call("t2", "mcp__docs__search", json!({ "query": "loaded" })));
    s.h.provider.push(FauxResponse::text("done"));
    s.h.session.prompt("go", Vec::new()).await.unwrap();

    assert_eq!(s.h.session.active_tool_names(), ["tool_search", "mcp__docs__search"]);
    assert_eq!(result_text(&tool_result(&s.h.session, "mcp__docs__search")), "loaded guide\nloaded faq");
    assert!(s.notifications.lock().unwrap().is_empty());
}

#[tokio::test]
async fn reaches_deferred_mcp_tools_through_codemode_when_tool_search_is_off() {
    let options = SetupOptions {
        default_tools: vec!["codemode"],
        settings: json!({ "extensions": ["-builtin:tool-search"] }),
        ..SetupOptions::default()
    };
    let s = setup(json!({ "exposure": "deferred" }), options).await;
    s.h.provider.push(FauxResponse::text("ready"));
    s.h.session.prompt("start", Vec::new()).await.unwrap();

    assert!(!s.h.session.builtin_extension_enabled("tool-search"));
    assert_eq!(s.h.session.active_tool_names(), ["codemode"]);
    assert!(s.h.session.callable_tool_names().contains(&"mcp__docs__search".to_string()));
    assert!(s.notifications.lock().unwrap().is_empty());
}

#[tokio::test]
async fn warns_when_deferred_mcp_tools_have_neither_tool_search_nor_codemode() {
    let settings = json!({ "extensions": ["-builtin:tool-search"] });
    let s = setup(json!({ "exposure": "deferred" }), SetupOptions { settings, ..SetupOptions::default() }).await;
    s.h.provider.push(FauxResponse::text("ready"));
    s.h.session.prompt("start", Vec::new()).await.unwrap();

    assert!(s.h.session.active_tool_names().is_empty());
    assert_eq!(
        *s.notifications.lock().unwrap(),
        [
            "MCP tools are only reachable from the codemode or tool_search tool, but neither is active; they cannot be called."
        ]
    );
}

struct OtherCodemode;

#[async_trait]
impl AgentTool for OtherCodemode {
    fn name(&self) -> &str {
        "codemode"
    }
    fn description(&self) -> &str {
        "Another codemode tool."
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    fn default_active(&self) -> bool {
        false
    }
    async fn execute(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
        _: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        Ok(AgentToolResult::text("other"))
    }
}

#[tokio::test]
async fn does_not_activate_another_tool_named_codemode() {
    let s =
        setup(json!({ "exposure": "codemode" }), SetupOptions { wait_for_tools: false, ..Default::default() }).await;
    s.h.session.register_tool(Arc::new(OtherCodemode), pi_coding_agent::agent_session::ToolSource::sdk("codemode"));
    s.h.session.set_active_tools_by_name(&[]);
    s.h.provider.push(FauxResponse::text("ready"));
    s.h.session.prompt("start", Vec::new()).await.unwrap();

    assert!(s.h.session.active_tool_names().is_empty());
}

#[tokio::test]
async fn does_not_hold_the_first_prompt_for_codemode_servers_still_connecting() {
    let options = SetupOptions { fake: slow(Duration::MAX), wait_for_tools: false, ..SetupOptions::default() };
    let s = setup(json!({}), options).await;
    s.h.provider.push(FauxResponse::text("ready"));
    s.h.session.prompt("start", Vec::new()).await.unwrap();

    assert_eq!(s.h.session.last_assistant_text().as_deref(), Some("ready"));
    assert_eq!(declared_tool_names(&s.h.session), ["codemode"]);
}

#[tokio::test]
async fn lists_a_server_before_it_connects_and_appends_its_summary_with_the_next_prompt() {
    let fake = FakeOptions { initialize_delay: Some(Duration::from_millis(50)), ..instructions("Slow docs.\nMore.") };
    let s = setup(json!({}), SetupOptions { fake, wait_for_tools: false, ..SetupOptions::default() }).await;
    s.h.provider.push(FauxResponse::text("one"));
    s.h.provider.push(FauxResponse::text("two"));
    s.h.session.prompt("first", Vec::new()).await.unwrap();
    let session = &s.h.session;
    wait_until(|| session.callable_tool_names().contains(&"mcp__docs__search".to_string())).await;
    session.prompt("second", Vec::new()).await.unwrap();

    let messages = session.messages();
    let systems: Vec<(usize, &pi_ai::SystemMessage)> = messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| match message {
            Message::System(system) => Some((index, system)),
            _ => None,
        })
        .collect();
    assert_eq!(systems.len(), 2);
    let section = |system: &pi_ai::SystemMessage| system.sections.as_ref().unwrap()["mcp_servers"].clone();
    assert!(section(systems[0].1).as_str().unwrap().contains("- mcp__docs (codemode)\n"));
    let patch = systems[1].1.sections.as_ref().unwrap();
    assert_eq!(patch.keys().collect::<Vec<_>>(), ["mcp_servers"]);
    assert!(section(systems[1].1).as_str().unwrap().contains("- mcp__docs (codemode): Slow docs."));
    let first_assistant = messages.iter().position(|message| matches!(message, Message::Assistant(_))).unwrap();
    assert!(systems[1].0 > first_assistant);
}

#[tokio::test]
async fn waits_for_servers_with_direct_tools_before_listing_the_servers() {
    let fake = FakeOptions { initialize_delay: Some(Duration::from_millis(30)), ..instructions("Slow docs.") };
    let config = json!({ "exposure": "direct", "toolExposure": { "shot": "codemode" } });
    let s = setup(config, SetupOptions { fake, wait_for_tools: false, ..SetupOptions::default() }).await;
    s.h.provider.push(FauxResponse::text("ready"));
    s.h.session.prompt("start", Vec::new()).await.unwrap();

    assert!(servers_section(&s.h.session).unwrap().contains("- mcp__docs (codemode): Slow docs."));
    assert!(declared_tool_names(&s.h.session).contains(&"mcp__docs__search".to_string()));
}

#[tokio::test]
async fn leaves_servers_with_only_direct_tools_out_of_the_servers_section() {
    let s = setup(json!({ "exposure": "direct" }), SetupOptions { wait_for_tools: false, ..Default::default() }).await;
    s.h.provider.push(FauxResponse::text("ready"));
    s.h.session.prompt("start", Vec::new()).await.unwrap();
    assert_eq!(servers_section(&s.h.session), None);
}

#[tokio::test]
async fn waits_for_the_servers_a_codemode_script_names() {
    let options = SetupOptions { fake: slow(Duration::from_millis(30)), wait_for_tools: false, ..Default::default() };
    let s = setup(json!({}), options).await;
    let code = r#"return (await tools.mcp__docs__search({ query: "q" })).structuredContent;"#;
    s.h.provider.push(FauxResponse::tool_call("c1", "codemode", json!({ "code": code })));
    s.h.provider.push(FauxResponse::text("done"));
    s.h.session.prompt("go", Vec::new()).await.unwrap();

    let result = tool_result(&s.h.session, "codemode");
    assert!(!result.is_error, "{}", result_text(&result));
    assert_eq!(serde_json::from_str::<Value>(&last_line(&result)).unwrap(), json!({ "hits": ["q guide", "q faq"] }));
    assert_eq!(s.fake.calls(), [r#"search:{"query":"q"}"#]);
}

#[tokio::test]
async fn does_not_wait_for_servers_a_codemode_script_does_not_name() {
    let options = SetupOptions { fake: slow(Duration::MAX), wait_for_tools: false, ..Default::default() };
    let s = setup(json!({}), options).await;
    s.h.provider.push(FauxResponse::tool_call("c1", "codemode", json!({ "code": "return 1 + 1;" })));
    s.h.provider.push(FauxResponse::text("done"));
    s.h.session.prompt("go", Vec::new()).await.unwrap();
    assert_eq!(last_line(&tool_result(&s.h.session, "codemode")), "2");
}

#[tokio::test]
async fn waits_for_servers_before_tool_search_searches() {
    let options = SetupOptions { fake: slow(Duration::from_millis(30)), wait_for_tools: false, ..Default::default() };
    let s = setup(json!({ "exposure": "deferred" }), options).await;
    s.h.provider.push(FauxResponse::tool_call("t1", "tool_search", json!({ "query": "search the docs", "limit": 1 })));
    s.h.provider.push(FauxResponse::tool_call("t2", "mcp__docs__search", json!({ "query": "late" })));
    s.h.provider.push(FauxResponse::text("done"));
    s.h.session.prompt("go", Vec::new()).await.unwrap();
    assert_eq!(result_text(&tool_result(&s.h.session, "mcp__docs__search")), "late guide\nlate faq");
}

#[tokio::test]
async fn holds_the_first_prompt_for_direct_servers_only_up_to_the_startup_wait() {
    let options = SetupOptions {
        fake: slow(Duration::MAX),
        extra: json!({ "startupWaitMs": 20 }),
        wait_for_tools: false,
        ..SetupOptions::default()
    };
    let s = setup(json!({ "exposure": "direct" }), options).await;
    s.h.provider.push(FauxResponse::text("ready"));
    s.h.session.prompt("start", Vec::new()).await.unwrap();

    assert_eq!(s.h.session.last_assistant_text().as_deref(), Some("ready"));
    assert!(s.h.session.active_tool_names().is_empty());
    assert_eq!(
        *s.notifications.lock().unwrap(),
        ["MCP servers are still connecting; their tools become available once connected."]
    );
}

#[tokio::test]
async fn does_not_let_codemode_call_itself_or_inactive_direct_tools() {
    let s = setup(json!({ "exposure": "direct" }), SetupOptions::default()).await;
    s.h.provider.push(FauxResponse::text("ready"));
    s.h.session.prompt("start", Vec::new()).await.unwrap();
    s.h.session.set_active_tools_by_name(&["codemode".into(), "mcp__docs__search".into()]);
    assert_eq!(s.h.session.callable_tool_names(), ["mcp__docs__search"]);
}

#[tokio::test]
async fn finds_tools_from_scripts_with_search_tools_and_describe_tool() {
    let s = setup(json!({ "exposure": "codemode" }), SetupOptions::default()).await;
    let code = r#"
const [match] = await searchTools("search the docs", { limit: 1 });
const none = await searchTools("docs", { namespace: "mcp__other" });
const declaration = await describeTool(match.name);
const result = await tools[match.name]({ query: "found" });
text(JSON.stringify({
  name: match.name,
  none: none.length,
  declared: declaration.includes("codemode tool declaration:"),
  missing: (await describeTool("nope")) === undefined,
  hits: result.structuredContent.hits,
}));"#;
    s.h.provider.push(FauxResponse::tool_call("c1", "codemode", json!({ "code": code })));
    s.h.provider.push(FauxResponse::text("done"));
    s.h.session.prompt("go", Vec::new()).await.unwrap();

    let result = tool_result(&s.h.session, "codemode");
    assert!(!result.is_error, "{}", result_text(&result));
    assert_eq!(
        serde_json::from_str::<Value>(&last_line(&result)).unwrap(),
        json!({ "name": "mcp__docs__search", "none": 0, "declared": true, "missing": true, "hits": ["found guide", "found faq"] })
    );
}
