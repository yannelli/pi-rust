//! Built-in MCP support of a session (upstream `test/suite/agent-session-mcp.test.ts`): tool
//! exposure, `--tools` and `--exclude-tools`, codemode and tool_search reachability, resources,
//! and per-tool exposure.

use pi_agent::ToolAnnotations;
use pi_ai::ContentBlock;
use pi_ai::providers::faux::FauxResponse;
use serde_json::{Value, json};

use crate::mcp_fake::{FakeOptions, TINY_PNG};
use crate::tool_support::{
    SetupOptions, declared_description, declared_tool_names, last_line, result_text, setup, tool_result,
};

#[tokio::test]
async fn keeps_mcp_tools_when_tools_names_no_mcp_tool() {
    for exposure in ["codemode", "deferred"] {
        let fake = FakeOptions { resources: true, ..FakeOptions::default() };
        let options = SetupOptions { tools: Some(vec!["read", "codemode"]), fake, ..SetupOptions::default() };
        let s = setup(json!({ "exposure": exposure }), options).await;
        let session = &s.h.session;
        crate::harness::wait_until(|| session.callable_tool_names().contains(&"read_mcp_resource".to_string())).await;
        assert_eq!(session.active_tool_names(), ["read", "codemode"]);
        let callable = session.callable_tool_names();
        for name in ["mcp__docs__search", "mcp__docs__fail", "list_mcp_resources", "read_mcp_resource"] {
            assert!(callable.contains(&name.to_string()), "{exposure}: {callable:?}");
        }
        assert!(!session.all_tools().iter().any(|tool| tool.name == "bash"));
    }

    // A direct MCP tool stays registered but is declared only when --tools names it.
    let options = SetupOptions { tools: Some(vec!["codemode"]), ..SetupOptions::default() };
    let s = setup(json!({ "exposure": "codemode", "toolExposure": { "fail": "direct" } }), options).await;
    crate::harness::wait_until(|| s.h.session.all_tools().iter().any(|tool| tool.name == "mcp__docs__fail")).await;
    assert_eq!(s.h.session.active_tool_names(), ["codemode"]);
}

#[tokio::test]
async fn removes_mcp_tools_with_an_empty_tools_list() {
    let options = SetupOptions { tools: Some(vec![]), wait_for_tools: false, ..SetupOptions::default() };
    let s = setup(json!({ "exposure": "direct" }), options).await;
    s.h.provider.push(FauxResponse::text("done"));
    s.h.session.prompt("go", Vec::new()).await.unwrap();

    assert_eq!(s.fake.connected.lock().unwrap().len(), 1);
    assert!(s.h.session.all_tools().is_empty());
    assert!(s.h.session.active_tool_names().is_empty());
}

#[tokio::test]
async fn filters_mcp_tools_by_the_mcp_entries_of_tools() {
    let fake = FakeOptions { resources: true, ..FakeOptions::default() };
    let options = SetupOptions { tools: Some(vec!["codemode", "mcp__docs__s*"]), fake, ..SetupOptions::default() };
    let s = setup(json!({ "exposure": "codemode", "toolExposure": { "shot": "direct" } }), options).await;
    let session = &s.h.session;
    crate::harness::wait_until(|| session.active_tool_names() == ["codemode", "mcp__docs__shot"]).await;
    let registered: Vec<String> = session.all_tools().into_iter().map(|tool| tool.name).collect();
    assert!(registered.contains(&"mcp__docs__search".into()) && registered.contains(&"mcp__docs__shot".into()));
    assert!(!registered.contains(&"mcp__docs__fail".into()) && !registered.contains(&"list_mcp_resources".into()));
}

#[tokio::test]
async fn removes_mcp_tools_matching_exclude_tools_patterns() {
    let options =
        SetupOptions { tools: Some(vec!["codemode"]), exclude: vec!["mcp__docs__f*"], ..SetupOptions::default() };
    let s = setup(json!({ "exposure": "codemode" }), options).await;
    let registered: Vec<String> = s.h.session.all_tools().into_iter().map(|tool| tool.name).collect();
    assert!(registered.contains(&"mcp__docs__search".into()));
    assert!(!registered.contains(&"mcp__docs__fail".into()));
}

#[tokio::test]
async fn exposes_codemode_only_mcp_tools_through_codemode_and_records_nested_calls() {
    let s = setup(json!({ "exposure": "codemode" }), SetupOptions::default()).await;
    let code = r#"
const [a, b] = await Promise.allSettled([
  tools.mcp__docs__search({ query: "mcp" }),
  tools.mcp__docs__search({ query: "pi" }),
]);
const failure = await tools.mcp__docs__fail({});
const shot = await tools.mcp__docs__shot({});
image(shot.content[0]);
text(JSON.stringify({
  hits: [...a.value.structuredContent.hits, ...b.value.structuredContent.hits],
  failed: failure.isError,
  failure: failure.content[0].text,
  found: ALL_TOOLS.filter((tool) => tool.name.includes("search")).map((tool) => tool.name),
}));"#;
    s.h.provider.push(FauxResponse::tool_call("c1", "codemode", json!({ "code": code })));
    s.h.provider.push(FauxResponse::text("done"));
    s.h.session.prompt("search the docs", Vec::new()).await.unwrap();

    let session = &s.h.session;
    assert_eq!(session.active_tool_names(), ["codemode"]);
    assert_eq!(declared_tool_names(session), ["codemode"]);
    assert_eq!(session.callable_tool_names(), ["mcp__docs__search", "mcp__docs__fail", "mcp__docs__shot"]);
    let description = declared_description(session, "codemode").unwrap();
    assert!(!description.contains("mcp__docs") && !description.contains("Shared MCP Types:"), "{description}");

    let result = tool_result(session, "codemode");
    assert!(!result.is_error, "{}", result_text(&result));
    assert_eq!(result.content.len(), 4, "{:?}", result.content);
    let saved = result.content[1].as_text().unwrap().to_string();
    let path = saved.strip_prefix("[Image saved to ").and_then(|rest| rest.split(' ').next()).unwrap();
    let _ = std::fs::remove_file(path);
    assert_eq!(result.content[2], ContentBlock::Image { data: TINY_PNG.into(), mime_type: "image/png".into() });
    let summary: Value = serde_json::from_str(result.content[3].as_text().unwrap()).unwrap();
    assert_eq!(
        summary,
        json!({ "hits": ["mcp guide", "mcp faq", "pi guide", "pi faq"], "failed": true,
            "failure": "server exploded", "found": ["mcp__docs__search"] })
    );
    assert_eq!(s.fake.calls(), [r#"search:{"query":"mcp"}"#, r#"search:{"query":"pi"}"#, "fail:{}", "shot:{}"]);

    // Nested calls run through the tool pipeline and are recorded on the codemode result.
    let nested = result.nested_calls.expect("nested calls");
    let names: Vec<&str> = nested.calls.iter().map(|call| call.name.as_str()).collect();
    assert_eq!(names, ["mcp__docs__search", "mcp__docs__search", "mcp__docs__fail", "mcp__docs__shot"]);
    assert!(nested.complete && nested.calls.iter().all(|call| call.id.starts_with("c1/")));
    let events = s.h.events_of("tool_execution_start");
    assert!(events.iter().any(|event| event["parentToolCallId"] == "c1" && event["toolName"] == "mcp__docs__fail"));
}

#[tokio::test]
async fn rejects_direct_model_calls_to_codemode_only_mcp_tools() {
    let s = setup(json!({ "exposure": "codemode" }), SetupOptions::default()).await;
    s.h.provider.push(FauxResponse::tool_call("c1", "mcp__docs__search", json!({ "query": "x" })));
    s.h.provider.push(FauxResponse::text("done"));
    s.h.session.prompt("go", Vec::new()).await.unwrap();

    let result = tool_result(&s.h.session, "mcp__docs__search");
    assert!(result.is_error);
    assert_eq!(result_text(&result), "Tool mcp__docs__search not found");
    assert!(s.fake.calls().is_empty());
}

#[tokio::test]
async fn declares_directly_exposed_mcp_tools_to_the_model() {
    let s = setup(json!({ "exposure": "direct" }), SetupOptions::default()).await;
    s.h.provider.push(FauxResponse::tool_call("c1", "mcp__docs__search", json!({ "query": "direct" })));
    s.h.provider.push(FauxResponse::text("done"));
    s.h.session.prompt("go", Vec::new()).await.unwrap();

    let session = &s.h.session;
    assert_eq!(declared_tool_names(session), ["mcp__docs__search", "mcp__docs__fail", "mcp__docs__shot"]);
    assert!(!session.active_tool_names().contains(&"codemode".to_string()));
    let tools = session.all_tools();
    let annotations = |name: &str| tools.iter().find(|tool| tool.name == name).unwrap().annotations.clone();
    let expected = ToolAnnotations { destructive_hint: Some(true), read_only_hint: Some(false), ..Default::default() };
    assert_eq!(annotations("mcp__docs__fail"), Some(expected));
    assert_eq!(annotations("mcp__docs__search"), None);
    let source = tools.iter().find(|tool| tool.name == "mcp__docs__search").unwrap();
    assert_eq!((source.source.as_str(), source.source_path.as_str()), ("builtin", "builtin:mcp"));
    assert_eq!(result_text(&tool_result(session, "mcp__docs__search")), "direct guide\ndirect faq");
}

#[tokio::test]
async fn withdraws_and_restores_mcp_tools_the_server_changes() {
    for exposure in ["direct", "codemode"] {
        let s = setup(json!({ "exposure": exposure }), SetupOptions::default()).await;
        s.h.provider.push(FauxResponse::text("ready"));
        s.h.session.prompt("start", Vec::new()).await.unwrap();
        let reachable = || {
            if exposure == "direct" { s.h.session.active_tool_names() } else { s.h.session.callable_tool_names() }
        };
        assert!(reachable().contains(&"mcp__docs__fail".to_string()));

        let without_fail: Vec<Value> = crate::mcp_fake::server_tools()
            .as_array()
            .unwrap()
            .iter()
            .filter(|tool| tool["name"] != "fail")
            .cloned()
            .collect();
        *s.fake.tools.lock().unwrap() = Value::Array(without_fail);
        s.fake.list_changed().await;
        crate::harness::wait_until(|| !reachable().contains(&"mcp__docs__fail".to_string())).await;
        assert!(reachable().contains(&"mcp__docs__search".to_string()));
        let description = declared_description(&s.h.session, "codemode").unwrap_or_default();
        assert!(!description.contains("mcp__docs__fail"));

        *s.fake.tools.lock().unwrap() = crate::mcp_fake::server_tools();
        s.fake.list_changed().await;
        crate::harness::wait_until(|| reachable().contains(&"mcp__docs__fail".to_string())).await;
    }
}

#[tokio::test]
async fn lists_and_reads_resources_with_the_resource_tools() {
    let fake = FakeOptions { resources: true, ..FakeOptions::default() };
    let s = setup(json!({ "exposure": "direct" }), SetupOptions { fake, ..SetupOptions::default() }).await;
    let session = &s.h.session;
    crate::harness::wait_until(|| session.active_tool_names().contains(&"read_mcp_resource".to_string())).await;
    s.h.provider.push(FauxResponse::tool_calls(&[
        ("r1", "list_mcp_resources", json!({})),
        ("r2", "list_mcp_resource_templates", json!({ "server": "docs" })),
        ("r3", "read_mcp_resource", json!({ "server": "docs", "uri": "docs://pages/setup" })),
    ]));
    s.h.provider.push(FauxResponse::tool_call("r4", "read_mcp_resource", json!({ "server": "nope", "uri": "x" })));
    s.h.provider.push(FauxResponse::text("done"));
    session.prompt("read", Vec::new()).await.unwrap();

    let listed: Value = serde_json::from_str(&result_text(&tool_result(session, "list_mcp_resources"))).unwrap();
    assert_eq!(
        listed,
        json!({ "resources": [{ "server": "docs", "uri": "docs://intro", "name": "intro", "mimeType": "text/markdown" }] })
    );
    let messages = session.messages();
    let reads: Vec<_> = messages
        .iter()
        .filter_map(|message| match message {
            pi_ai::Message::ToolResult(result) if result.tool_name == "read_mcp_resource" => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(result_text(reads[0]), "# docs://pages/setup");
    assert!(reads[1].is_error);
    assert_eq!(result_text(reads[1]), "MCP server \"nope\" has no resources. Servers with resources: docs");
    assert_eq!(s.fake.calls(), ["read:docs://pages/setup"]);
}

#[tokio::test]
async fn makes_the_resource_tools_callable_from_codemode_for_codemode_servers() {
    let fake = FakeOptions { resources: true, ..FakeOptions::default() };
    let s = setup(json!({ "exposure": "codemode" }), SetupOptions { fake, ..SetupOptions::default() }).await;
    crate::harness::wait_until(|| s.h.session.callable_tool_names().contains(&"read_mcp_resource".into())).await;
    let code = r#"const listed = await tools.list_mcp_resources({ server: "docs" });
const read = await tools.read_mcp_resource({ server: "docs", uri: listed.resources[0].uri });
return { uris: listed.resources.map((r) => r.uri), text: read.contents[0].text };"#;
    s.h.provider.push(FauxResponse::tool_call("c1", "codemode", json!({ "code": code })));
    s.h.provider.push(FauxResponse::text("done"));
    s.h.session.prompt("read", Vec::new()).await.unwrap();

    assert_eq!(s.h.session.active_tool_names(), ["codemode"]);
    let returned: Value = serde_json::from_str(&last_line(&tool_result(&s.h.session, "codemode"))).unwrap();
    assert_eq!(returned, json!({ "uris": ["docs://intro"], "text": "# docs://intro" }));
    assert_eq!(s.fake.calls(), ["read:docs://intro"]);
}

#[tokio::test]
async fn applies_per_tool_exposure_overrides() {
    let config = json!({ "exposure": "hidden", "toolExposure": { "search": "direct", "s*": "codemode" } });
    let options = SetupOptions { wait_for_tools: false, ..SetupOptions::default() };
    let s = setup(config, options).await;
    crate::harness::wait_until(|| s.h.session.active_tool_names().contains(&"mcp__docs__search".into())).await;
    s.h.provider.push(FauxResponse::text("ready"));
    s.h.session.prompt("start", Vec::new()).await.unwrap();

    let session = &s.h.session;
    assert_eq!(declared_tool_names(session), ["codemode", "mcp__docs__search"]);
    assert_eq!(session.callable_tool_names(), ["mcp__docs__search", "mcp__docs__shot"]);
    let description = declared_description(session, "codemode").unwrap();
    assert!(!description.contains("mcp__docs__shot") && !description.contains("mcp__docs__fail"));
}

#[tokio::test]
async fn mcp_status_and_actions_reach_the_session() {
    let s = setup(json!({ "exposure": "direct" }), SetupOptions::default()).await;
    let session = &s.h.session;
    session.mcp_pending().await;
    assert_eq!(session.mcp_status_text(), "docs: connected, 3 tools (direct)");
    let statuses = session.mcp_statuses();
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].tools, ["search", "fail", "shot"]);
    assert!(session.active_tool_names().contains(&"mcp__docs__search".to_string()));

    // `/mcp` without a UI reports through the notify handler.
    s.notifications.lock().unwrap().clear();
    let disposition = session.prompt("/mcp", Vec::new()).await.unwrap();
    assert_eq!(disposition.as_str(), "handled");
    assert_eq!(*s.notifications.lock().unwrap(), ["docs: connected, 3 tools (direct)"]);
    session.prompt("/mcp reconnect", Vec::new()).await.unwrap();
    let last = s.notifications.lock().unwrap().last().cloned().unwrap();
    assert_eq!(last, "Reconnected to MCP server \"docs\" (connected · 3 tools).");
    session.prompt("/mcp login", Vec::new()).await.unwrap();
    let last = s.notifications.lock().unwrap().last().cloned().unwrap();
    assert_eq!(last, "Signing in to MCP server \"docs\" requires interactive mode.");
    // Tools that stop being direct leave the declared set, and codemode takes them over.
    assert_eq!(session.mcp_set_exposure("docs", pi_mcp::McpExposure::Codemode), None);
    let active = session.active_tool_names();
    assert_eq!(active, ["codemode"]);
    assert!(session.callable_tool_names().contains(&"mcp__docs__search".to_string()));
    session.prompt("/mcp logout docs", Vec::new()).await.unwrap();
    let last = s.notifications.lock().unwrap().last().cloned().unwrap();
    assert_eq!(last, "No stored credentials for MCP server \"docs\".");
    assert!(s.h.requests().is_empty(), "/mcp never reaches the model");

    session.shutdown().await;
    assert!(session.mcp_manager().is_none());
    assert!(!session.all_tools().iter().any(|tool| tool.name.starts_with("mcp__")));
}
