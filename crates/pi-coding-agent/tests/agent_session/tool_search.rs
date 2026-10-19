//! `tool_search` in a session (upstream `agent-session-mcp.test.ts` tool_search cases and the
//! "after resume and reload" suite): loads are declared in system messages, follow the branch,
//! and come back on resume and after `/reload` once their server connects again.

use std::time::Duration;

use pi_ai::Message;
use pi_ai::providers::faux::FauxResponse;
use pi_coding_agent::agent_session::NavigateTreeOptions;
use pi_coding_agent::session::SessionManager;
use serde_json::json;

use crate::harness::{open_session_with, wait_until};
use crate::mcp_fake::{Fake, FakeOptions};
use crate::tool_support::{SetupOptions, declared_description, result_text, setup, tool_result};

fn load_responses(provider: &pi_ai::providers::faux::FauxProvider) {
    provider.push(FauxResponse::tool_call("t1", "tool_search", json!({ "query": "search the docs", "limit": 1 })));
    provider.push(FauxResponse::text("loaded"));
}

#[tokio::test]
async fn activates_tool_search_for_deferred_mcp_tools_and_keeps_loads_on_the_branch() {
    let s = setup(json!({ "exposure": "deferred" }), SetupOptions::default()).await;
    s.h.provider.push(FauxResponse::tool_call("t1", "tool_search", json!({ "query": "search the docs", "limit": 1 })));
    s.h.provider.push(FauxResponse::tool_call("t2", "mcp__docs__search", json!({ "query": "loaded" })));
    s.h.provider.push(FauxResponse::text("done"));
    let session = &s.h.session;
    session.prompt("find a docs tool", Vec::new()).await.unwrap();

    assert_eq!(session.active_tool_names(), ["tool_search", "mcp__docs__search"]);
    assert_eq!(declared_description(session, "tool_search").unwrap(), pi_codemode::TOOL_SEARCH_DESCRIPTION);
    let search = tool_result(session, "tool_search");
    assert_eq!(
        result_text(&search),
        "Loaded 1 tool. They are available from your next call:\n- mcp__docs__search: Search the docs."
    );
    assert_eq!(search.details, Some(json!({ "loaded": ["mcp__docs__search"] })));
    // The load is a tool delta in a system message; earlier declarations are not repeated.
    let loads: Vec<Vec<String>> = session
        .messages()
        .iter()
        .filter_map(|message| match message {
            Message::System(system) => system.tools_added.clone(),
            _ => None,
        })
        .map(|tools| tools.into_iter().map(|tool| tool.name).collect::<Vec<_>>())
        .filter(|names| names.contains(&"mcp__docs__search".to_string()))
        .collect();
    assert_eq!(loads, [["mcp__docs__search"]]);
    assert_eq!(result_text(&tool_result(session, "mcp__docs__search")), "loaded guide\nloaded faq");
    assert_eq!(s.fake.calls(), [r#"search:{"query":"loaded"}"#]);

    // Navigating before the load drops the tool; navigating to a later entry restores it.
    let (first_user, last) = {
        let manager = session.session_manager();
        let branch = manager.get_branch(None);
        let user = branch
            .iter()
            .find(|entry| matches!(entry.data(), pi_coding_agent::session::EntryData::Message(Message::User(_))));
        (user.unwrap().id().to_string(), branch.last().unwrap().id().to_string())
    };
    session.navigate_tree(&first_user, NavigateTreeOptions::default()).await.unwrap();
    assert!(!session.active_tool_names().contains(&"mcp__docs__search".to_string()));
    session.navigate_tree(&last, NavigateTreeOptions::default()).await.unwrap();
    assert!(session.active_tool_names().contains(&"mcp__docs__search".to_string()));
}

#[tokio::test]
async fn finds_nothing_to_load_when_every_matching_tool_is_declared() {
    let options = SetupOptions { default_tools: vec!["tool_search"], ..SetupOptions::default() };
    let s = setup(json!({ "exposure": "direct" }), options).await;
    s.h.provider.push(FauxResponse::tool_call("t1", "tool_search", json!({ "query": "docs" })));
    s.h.provider.push(FauxResponse::text("done"));
    s.h.session.prompt("go", Vec::new()).await.unwrap();
    assert_eq!(result_text(&tool_result(&s.h.session, "tool_search")), "No matching tools found.");
}

#[tokio::test]
async fn rejects_empty_queries_and_bad_limits() {
    let options = SetupOptions { default_tools: vec!["tool_search"], ..SetupOptions::default() };
    let s = setup(json!({ "exposure": "deferred" }), options).await;
    s.h.provider.push(FauxResponse::tool_calls(&[
        ("t1", "tool_search", json!({ "query": "  " })),
        ("t2", "tool_search", json!({ "query": "docs", "limit": 1.5 })),
    ]));
    s.h.provider.push(FauxResponse::text("done"));
    s.h.session.prompt("go", Vec::new()).await.unwrap();
    let errors: Vec<String> =
        s.h.session
            .messages()
            .iter()
            .filter_map(|message| match message {
                Message::ToolResult(result) if result.is_error => Some(result_text(result)),
                _ => None,
            })
            .collect();
    assert_eq!(errors, ["query must not be empty", "limit must be a positive integer"]);
}

/// A second session over the stored file of `first`, restoring its tools, with a server that
/// answers `initialize` after `delay`.
async fn resume(
    first: &crate::tool_support::Setup,
    delay: Duration,
) -> (std::sync::Arc<pi_coding_agent::agent_session::AgentSession>, Fake) {
    let fake = Fake::new(FakeOptions { initialize_delay: Some(delay), ..FakeOptions::default() });
    let mcp =
        fake.session_options("docs", json!({ "url": "http://unused.invalid", "exposure": "deferred" }), json!({}));
    let file = first.h.session.session_file().unwrap();
    let manager = SessionManager::open(&file).unwrap();
    let session = open_session_with(&first.h.services, manager, |create| create.mcp = mcp).await;
    (session, fake)
}

#[tokio::test]
async fn declares_tools_tool_search_loaded_again_on_resume_once_their_server_connects() {
    let first = setup(json!({ "exposure": "deferred" }), SetupOptions::default()).await;
    load_responses(&first.h.provider);
    first.h.session.prompt("load", Vec::new()).await.unwrap();
    assert!(first.h.session.active_tool_names().contains(&"mcp__docs__search".to_string()));

    let (second, fake) = resume(&first, Duration::from_millis(20)).await;
    wait_until(|| second.active_tool_names().contains(&"mcp__docs__search".to_string())).await;
    first.h.provider.push(FauxResponse::tool_call("c2", "mcp__docs__search", json!({ "query": "again" })));
    first.h.provider.push(FauxResponse::text("done"));
    second.prompt("use it", Vec::new()).await.unwrap();

    assert_eq!(result_text(&tool_result(&second, "mcp__docs__search")), "again guide\nagain faq");
    let removals = second.messages().iter().any(|message| {
        matches!(message, Message::System(system) if system.tools_removed.as_ref().is_some_and(|removed| !removed.is_empty()))
    });
    assert!(!removals, "the restored loadout is not redeclared");
    assert_eq!(*fake.connected.lock().unwrap(), ["docs"]);
}

#[tokio::test]
async fn does_not_activate_restored_tools_that_register_after_the_next_prompt_starts() {
    let first = setup(json!({ "exposure": "deferred" }), SetupOptions::default()).await;
    load_responses(&first.h.provider);
    first.h.session.prompt("load", Vec::new()).await.unwrap();

    let (second, _fake) = resume(&first, Duration::from_millis(200)).await;
    first.h.provider.push(FauxResponse::text("done"));
    second.prompt("go", Vec::new()).await.unwrap();
    wait_until(|| second.all_tools().iter().any(|tool| tool.name == "mcp__docs__search")).await;
    assert!(!second.active_tool_names().contains(&"mcp__docs__search".to_string()));
}

#[tokio::test]
async fn declares_tools_tool_search_loaded_again_after_reload() {
    let s = setup(json!({ "exposure": "deferred" }), SetupOptions::default()).await;
    load_responses(&s.h.provider);
    s.h.session.prompt("load", Vec::new()).await.unwrap();

    s.h.session.reload();
    wait_until(|| s.fake.connected.lock().unwrap().len() == 2).await;
    wait_until(|| s.h.session.active_tool_names().contains(&"mcp__docs__search".to_string())).await;
}
