//! Ported from upstream `coding-agent/test/tool-search.test.ts`: tokenizer, BM25 ranking, and the
//! catalog in the codemode description.

use std::collections::HashSet;

use pi_codemode::{
    Bm25Ranker, CodemodeDescriptionOptions, CodemodeToolInfo, ToolExposure, ToolNamespace, ToolRanker,
    create_codemode_description, create_tool_search_document, format_tool_search_result, search_deferred_tools,
    tokenize, tool_search_document, tool_search_limit,
};
use serde_json::{Value, json};

fn tool(name: &str, description: &str, properties: Value) -> CodemodeToolInfo {
    CodemodeToolInfo::new(name, description, json!({ "type": "object", "properties": properties }))
}

#[test]
fn tokenizes_camel_and_snake_case_without_stop_words_and_plurals() {
    assert_eq!(tokenize("listIssues for the GitHub_repo"), ["list", "issue", "git", "hub", "repo"]);
    assert_eq!(tokenize("searches queries HTTPServer"), ["search", "query", "http", "server"]);
}

fn documents() -> Vec<pi_codemode::ToolSearchDocument> {
    [
        tool(
            "mcp__github__list_issues",
            "List issues in a repository.",
            json!({ "state": { "type": "string", "description": "open or closed" } }),
        ),
        tool("mcp__github__create_pull_request", "Open a pull request.", json!({})),
        tool("mcp__linear__search_issues", "Search Linear issues by text.", json!({})),
        tool("mcp__docs__search", "Search the documentation.", json!({})),
    ]
    .iter()
    .map(tool_search_document)
    .collect()
}

fn names(matches: Vec<pi_codemode::ToolSearchMatch>) -> Vec<String> {
    matches.into_iter().map(|found| found.name).collect()
}

#[test]
fn ranks_by_term_relevance_and_respects_the_limit() {
    let ranker = Bm25Ranker::default();
    let documents = documents();
    assert_eq!(names(ranker.rank("issue", &documents, 8)), ["mcp__linear__search_issues", "mcp__github__list_issues"]);
    assert_eq!(ranker.rank("pull requests", &documents, 8)[0].name, "mcp__github__create_pull_request");
    assert_eq!(ranker.rank("search", &documents, 1).len(), 1);
    // Property names and descriptions are searchable.
    assert_eq!(names(ranker.rank("closed", &documents, 8)), ["mcp__github__list_issues"]);
}

// Upstream #11331.
#[test]
fn ranks_a_tool_first_when_the_query_is_its_name() {
    let named: Vec<_> = [
        tool("mcp__app__queue_read", "Read a queued message of this thread.", json!({})),
        tool(
            "mcp__dev-app__thread_read",
            "Read durable state and a paginated timeline from any thread. The default view returns user messages, assistant messages and plans. Continue with afterPosition until hasMore is false.",
            json!({}),
        ),
    ]
    .iter()
    .map(tool_search_document)
    .collect();
    let ranker = Bm25Ranker::default();
    for query in ["thread_read", "dev_app__thread_read", "mcp__dev-app__thread_read", "Thread_Read", "thread read"] {
        assert_eq!(names(ranker.rank(query, &named, 1)), ["mcp__dev-app__thread_read"], "{query}");
    }
    // Partial names still rank by BM25.
    assert_eq!(names(ranker.rank("read", &named, 1)), ["mcp__app__queue_read"]);
}

#[test]
fn returns_nothing_for_unknown_or_empty_queries() {
    let ranker = Bm25Ranker::default();
    let documents = documents();
    assert!(ranker.rank("kubernetes", &documents, 8).is_empty());
    assert!(ranker.rank("the", &documents, 8).is_empty());
    // No synonyms, so "tickets" does not find "issues".
    assert!(ranker.rank("tickets", &documents, 8).is_empty());
    assert!(ranker.rank("issue", &documents, 0).is_empty());
}

#[test]
fn includes_the_namespace_in_the_search_text() {
    let namespace =
        ToolNamespace { description: Some("Kubernetes cluster tools".into()), ..ToolNamespace::new("mcp__x") };
    let document =
        create_tool_search_document("mcp__x__run", "Run it.", &json!({ "type": "object" }), Some(&namespace));
    let matches = Bm25Ranker::default().rank("kubernetes", &[document], 8);
    assert_eq!(names(matches.clone()), ["mcp__x__run"]);
    assert!(matches[0].score > 0.0);
}

#[test]
fn searches_deferred_tools_that_are_not_active() {
    let tools = vec![
        (tool("mcp__github__list_issues", "List issues in a repository.\nMore.", json!({})), ToolExposure::Deferred),
        (tool("mcp__linear__search_issues", "Search Linear issues.", json!({})), ToolExposure::Codemode),
        (tool("read_issues", "Read issues.", json!({})), ToolExposure::Direct),
    ];
    let active = vec!["mcp__linear__search_issues".to_string()];
    let found = search_deferred_tools(&tools, &active, "issues", 8);
    assert_eq!(found.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>(), ["mcp__github__list_issues"]);
    assert_eq!(
        format_tool_search_result(&found),
        "Loaded 1 tool. They are available from your next call:\n- mcp__github__list_issues: List issues in a repository."
    );
    assert_eq!(format_tool_search_result(&[]), "No matching tools found.");
    assert_eq!(tool_search_limit("x", None), Ok(8));
    assert_eq!(tool_search_limit(" ", None), Err("query must not be empty".into()));
    assert_eq!(tool_search_limit("x", Some(1.5)), Err("limit must be a positive integer".into()));
}

fn catalog() -> Vec<CodemodeToolInfo> {
    let github = ToolNamespace { description: Some("GitHub server".into()), ..ToolNamespace::new("mcp__github") };
    let mut all = vec![tool("read_notes", "Read notes.", json!({}))];
    for suffix in ["a", "b", "c"] {
        let mut entry = tool(&format!("mcp__github__{suffix}"), &format!("GitHub {suffix}."), json!({}));
        entry.namespace = Some(github.clone());
        all.push(entry);
    }
    for (name, description) in
        [("mcp__docs__search", "Search docs.".to_string()), ("mcp__docs__long", "Long ".repeat(200))]
    {
        let mut entry = tool(name, &description, json!({}));
        entry.namespace = Some(ToolNamespace::new("mcp__docs"));
        all.push(entry);
    }
    all
}

fn describe(tools: &[CodemodeToolInfo], budget: Option<usize>, deferred: &[&str]) -> String {
    let options = CodemodeDescriptionOptions {
        inline_budget: budget,
        deferred: deferred.iter().map(|name| name.to_string()).collect::<HashSet<_>>(),
        models_docs_path: None,
    };
    create_codemode_description(tools, &options)
}

#[test]
fn says_that_tools_returning_an_object_also_resolve_on_failure() {
    let description = describe(&catalog(), None, &[]);
    assert!(description.contains(
        "and rejects with an Error on failure. A tool whose declaration returns an object resolves on failure too when it returned that object; check its fields. Calls still running when the script ends are cancelled."
    ));
}

#[test]
fn lists_everything_without_a_budget() {
    let description = describe(&catalog(), None, &[]);
    assert!(description.contains("Nested tools:"));
    assert!(description.contains("## mcp__github\nGitHub server"));
    assert!(description.contains("## mcp__docs\n\n### `mcp__docs"));
    assert!(description.contains("find unlisted tools, such as MCP tools"));
    assert!(description.contains("`await searchTools(query, { limit?, namespace? })`"));
    assert!(description.contains("`await describeTool(name)`"));
    assert!(description.contains("`await describeNamespace(name)`"));
    assert!(description.find("### `read_notes`").unwrap() < description.find("## mcp__docs").unwrap());
    assert!(description.find("## mcp__docs").unwrap() < description.find("## mcp__github").unwrap());
}

#[test]
fn fills_the_budget_round_robin_cheapest_first() {
    let description = describe(&catalog(), Some(170), &[]);
    assert!(description.contains("### `read_notes`"));
    assert!(description.contains("## mcp__docs (some tools not listed)"));
    assert!(description.contains("### `mcp__docs__search`"));
    assert!(!description.contains("### `mcp__docs__long`"));
    assert!(description.contains("## mcp__github (some tools not listed)"));
    assert_eq!(describe(&catalog(), Some(170), &[]), description);
}

#[test]
fn leaves_deferred_tools_and_their_namespaces_out() {
    let all = catalog();
    let github: Vec<&str> = all[1..4].iter().map(|tool| tool.name.as_str()).collect();
    let description = describe(&all, None, &github);
    assert!(!description.contains("mcp__github"));
    let without: Vec<CodemodeToolInfo> =
        all.iter().filter(|tool| !github.contains(&tool.name.as_str())).cloned().collect();
    assert_eq!(description, describe(&without, None, &[]));
}

#[test]
fn leaves_namespace_instructions_out() {
    let mut tools: Vec<CodemodeToolInfo> = catalog()[1..4].to_vec();
    for entry in &mut tools {
        entry.namespace =
            Some(ToolNamespace { instructions: Some("Long usage guide.".into()), ..ToolNamespace::new("mcp__github") });
    }
    let description = describe(&tools, None, &[]);
    assert!(description.contains("## mcp__github\n\n### `mcp__github__a`"));
    assert!(!description.contains("Long usage guide."));
}

#[test]
fn lists_only_namespaces_with_a_zero_budget() {
    let description = describe(&catalog(), Some(0), &[]);
    assert!(description.contains("## mcp__docs (tools not listed)"));
    assert!(!description.contains("codemode tool declaration:"));
}
