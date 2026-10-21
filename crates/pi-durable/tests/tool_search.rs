//! Port of upstream `test/tool-search.test.ts`: deferred tools and the `tool_search` extension.

mod support;

use std::sync::Arc;

use pi_ai::Message;
use pi_chord::Json;
use pi_durable::harness::agent::AGENT_DOC;
use pi_durable::harness::*;
use pi_durable::storage::MemoryStorage;
use pi_durable::testing::faux::FauxResponseStep;
use pi_durable::tool_search::create_tool_search_extension;
use pi_durable::{ConversationOwnership, EntryRecord};
use serde_json::{Value, json};
use support::chat::{add_tool, answer, call, calls, no_parameters};
use support::harness_support::*;

fn tool(name: &str, description: &str) -> ToolRegistration {
    let parameters = json!({
        "type": "object",
        "properties": { "number": { "type": "number", "description": "Issue number" } },
    });
    let text = format!("{name} ran");
    define_tool(name, description, parameters, move |_, _, _| {
        let text = text.clone();
        async move { Ok(ToolExecutionResult::text(text)) }
    })
}

fn search(query: &str, limit: Option<u32>, id: &str) -> FauxResponseStep {
    let args = match limit {
        Some(limit) => json!({ "query": query, "limit": limit }),
        None => json!({ "query": query }),
    };
    calls(&[("tool_search", args, id)])
}

fn done() -> FauxResponseStep {
    answer("done")
}

/// `alpha`, the deferred `github_get_issue` and `github_list_repos`, then `omega`, and `tool_search` last.
fn setup() -> ChatSetup {
    let chat = chat_setup(Default::default());
    add_tool(&chat, tool("alpha", "Alpha tool"));
    add_tool(&chat, tool("github_get_issue", "Get a GitHub issue by number").with_deferred());
    add_tool(&chat, tool("github_list_repos", "List the repositories of a user").with_deferred());
    add_tool(&chat, tool("omega", "Omega tool"));
    chat.registry.install(create_tool_search_extension()).unwrap();
    chat
}

fn names(tools: &[Arc<ToolRegistration>]) -> Vec<String> {
    tools.iter().map(|tool| tool.name().to_owned()).collect()
}

async fn offered(root: &Conversation) -> Vec<String> {
    names(&root.agent(&ctx()).await.unwrap().tools)
}

async fn deferred(root: &Conversation) -> Vec<String> {
    names(&root.agent(&ctx()).await.unwrap().deferred)
}

async fn submit_and_wait(root: &Conversation, text: &str) {
    root.submit(SubmissionDraft::input(text), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
}

async fn stored_model_tools(harness: &Harness, root: &Conversation) -> Option<Json> {
    let agent = harness.snapshot(&AGENT_DOC, root.id, &ctx()).await.unwrap();
    agent.and_then(|agent| agent.get("modelTools").cloned())
}

/// Names of the tools a `pi.system` entry adds and removes.
type ToolChanges = (Option<Vec<String>>, Option<Vec<String>>);

/// The tool changes of the `pi.system` entries, oldest first.
fn system_messages(entries: &[EntryRecord]) -> Vec<ToolChanges> {
    let system = entries.iter().filter(|entry| entry.kind == "pi.system");
    system
        .map(|entry| {
            let Some(Message::System(message)) = entry.model.as_ref().and_then(|model| model.first()) else {
                panic!("pi.system entry without a system message")
            };
            let added = message.tools_added.as_ref().map(|tools| tools.iter().map(|t| t.name.clone()).collect());
            let removed = message.tools_removed.as_ref().map(|tools| tools.iter().map(|t| t.name.clone()).collect());
            (added, removed)
        })
        .collect()
}

fn tool_result_texts(entries: &[EntryRecord]) -> Vec<String> {
    let results = entries.iter().filter(|entry| entry.kind == "pi.tool-result");
    results.map(|entry| text_of(entry.model.as_ref().and_then(|model| model.first())).unwrap_or_default()).collect()
}

fn strings(list: &[&str]) -> Option<Vec<String>> {
    Some(list.iter().map(|name| (*name).to_owned()).collect())
}

#[tokio::test]
async fn offers_a_deferred_tool_only_once_tool_search_loads_it_by_appending_its_declaration() {
    let chat = setup();
    let get_issue = calls(&[("github_get_issue", json!({ "number": 42 }), "c2")]);
    chat.faux.set_responses([search("github issue", Some(1), "c1"), get_issue, done()]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &chat).await;
    assert_eq!(offered(&root).await, ["alpha", "omega", "tool_search"]);
    assert_eq!(deferred(&root).await, ["github_get_issue", "github_list_repos"]);
    // Deferred tools stay callable by other tools, such as code mode scripts.
    assert!(names(&root.agent(&ctx()).await.unwrap().callable).contains(&"github_get_issue".to_owned()));
    submit_and_wait(&root, "go").await;
    let entries = all_entries(&root).await;
    let texts = tool_result_texts(&entries);
    let loaded =
        "Loaded 1 tool. They are available from your next call:\n- github_get_issue: Get a GitHub issue by number";
    assert_eq!(texts, [loaded, "github_get_issue ran"]);
    assert_eq!(stored_model_tools(&harness, &root).await, Some(json!({ "add": ["github_get_issue"] }).into()));
    assert_eq!(offered(&root).await, ["alpha", "omega", "tool_search", "github_get_issue"]);
    // A later search sees only the tools not loaded yet.
    assert_eq!(deferred(&root).await, ["github_list_repos"]);
    // The load appended the declaration: no tool was removed and declared again, so the cached prefix holds.
    let messages = system_messages(&entries);
    assert_eq!(messages[0].0, strings(&["alpha", "omega", "tool_search"]));
    assert_eq!(messages[1], (strings(&["github_get_issue"]), None));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn appends_each_loaded_tool_in_load_order_not_registry_order() {
    let chat = setup();
    chat.faux.set_responses([search("repositories", Some(1), "c1"), search("issue", Some(1), "c2"), done()]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &chat).await;
    submit_and_wait(&root, "go").await;
    let expected = ["alpha", "omega", "tool_search", "github_list_repos", "github_get_issue"];
    assert_eq!(offered(&root).await, expected);
    let loads: Vec<_> = system_messages(&all_entries(&root).await).into_iter().skip(1).collect();
    assert_eq!(loads, [(strings(&["github_list_repos"]), None), (strings(&["github_get_issue"]), None)]);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn records_loads_by_name_unknown_and_non_deferred_names_change_nothing_remove_wins_and_a_load_lifts_it() {
    let chat = setup();
    let grow = define_tool("grow", "grow", no_parameters(), |_, _, _| async {
        let added = vec!["github_get_issue".into(), "alpha".into(), "ghost".into()];
        let control = ToolControl { add_tools: Some(added), ..ToolControl::default() };
        Ok(ToolExecutionResult { output: Some(Vec::new()), control: Some(control), ..ToolExecutionResult::default() })
    });
    add_tool(&chat, grow);
    chat.faux.set_responses([call("grow", "c1"), done(), call("grow", "c2"), done()]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &chat).await;
    submit_and_wait(&root, "go").await;
    let stored: Json = json!({ "add": ["github_get_issue", "alpha", "ghost"] }).into();
    assert_eq!(stored_model_tools(&harness, &root).await, Some(stored.clone()));
    assert_eq!(offered(&root).await, ["alpha", "omega", "tool_search", "grow", "github_get_issue"]);
    // A name in both remove and add is not offered.
    let both = strings(&["github_get_issue"]);
    let edit = ModelToolFilter::Edit { remove: both.clone(), add: both };
    root.configure(AgentChange { model_tools: Change::Set(edit), ..AgentChange::default() }, &ctx()).await.unwrap();
    assert!(!offered(&root).await.contains(&"github_get_issue".to_owned()));
    // Loading it again drops it from remove.
    submit_and_wait(&root, "again").await;
    assert_eq!(stored_model_tools(&harness, &root).await, Some(stored));
    assert!(offered(&root).await.contains(&"github_get_issue".to_owned()));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn keeps_a_loaded_tool_when_its_extension_is_gone_for_a_while_and_appends_to_an_explicit_array() {
    let chat = setup();
    let issue = tool("github_get_issue", "Get a GitHub issue by number").with_deferred();
    chat.faux.set_responses([search("github issue", Some(1), "c1"), done()]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &chat).await;
    submit_and_wait(&root, "go").await;
    // The server goes away: its extension is installed again without the tool.
    chat.registry.install(Extension::new("tool:github_get_issue")).unwrap();
    assert!(!offered(&root).await.contains(&"github_get_issue".to_owned()));
    chat.registry.install(Extension::new("tool:github_get_issue").tool(issue)).unwrap();
    assert!(offered(&root).await.contains(&"github_get_issue".to_owned()));
    // With an explicit array, a load appends to it as before.
    let list = ModelToolFilter::List(vec!["alpha".into(), "tool_search".into()]);
    root.configure(AgentChange { model_tools: Change::Set(list), ..AgentChange::default() }, &ctx()).await.unwrap();
    chat.faux.set_responses([search("repositories", Some(1), "c1"), done()]);
    submit_and_wait(&root, "more").await;
    let stored: Value = json!(["alpha", "tool_search", "github_list_repos"]);
    assert_eq!(stored_model_tools(&harness, &root).await, Some(stored.into()));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn keeps_loaded_tools_per_branch_and_in_the_baseline_after_a_reset() {
    let chat = setup();
    chat.faux.set_responses([search("github issue", Some(1), "c1"), done()]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &chat).await;
    submit_and_wait(&root, "go").await;
    let entries = all_entries(&root).await;
    let ownerless = || CreateOptions::owned(ConversationOwnership::Ownerless);
    let before = root.fork(entries[0].id, ownerless(), &ctx()).await.unwrap();
    let after = root.fork(entries.last().unwrap().id, ownerless(), &ctx()).await.unwrap();
    assert!(!offered(&before).await.contains(&"github_get_issue".to_owned()));
    assert!(offered(&after).await.contains(&"github_get_issue".to_owned()));
    root.reset(None, &ctx()).await.unwrap();
    chat.faux.set_responses([done()]);
    submit_and_wait(&root, "fresh").await;
    let baseline = system_messages(&all_entries(&root).await).pop().unwrap();
    assert_eq!(baseline.0, strings(&["alpha", "omega", "tool_search", "github_get_issue"]));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn finds_nothing_for_an_unrelated_query_and_loads_nothing() {
    let chat = setup();
    chat.faux.set_responses([search("weather forecast", None, "c1"), done()]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &chat).await;
    submit_and_wait(&root, "go").await;
    assert_eq!(tool_result_texts(&all_entries(&root).await), ["No matching tools found."]);
    assert_eq!(stored_model_tools(&harness, &root).await, None);
    harness.close(&ctx()).await.unwrap();
}
