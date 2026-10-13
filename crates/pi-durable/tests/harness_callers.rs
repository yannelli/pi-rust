//! Port of upstream `test/harness-callers.test.ts` (who may call a tool).

mod support;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use pi_durable::harness::agent::AGENT_DOC;
use pi_durable::harness::*;
use pi_durable::storage::MemoryStorage;
use serde_json::json;
use support::chat::{add_tool, answer, call, no_parameters, text_tool};
use support::harness_support::*;

type Received = Arc<Mutex<HashMap<String, NestedToolExecutionResult>>>;

/// A tool that calls each of `names` as a nested call and records what it got back.
fn prober(names: &[&str]) -> (ToolRegistration, Received) {
    let received = Received::default();
    let (sink, names) = (received.clone(), names.iter().map(|name| (*name).to_owned()).collect::<Vec<_>>());
    let registration = define_tool("probe", "probe", no_parameters(), move |_, api, context| {
        let (sink, names) = (sink.clone(), names.clone());
        async move {
            for name in names {
                let result =
                    api.execute_tool(&name, Default::default(), &context, ExecuteToolOptions::default()).await?;
                sink.lock().unwrap().insert(name, result);
            }
            Ok(ToolExecutionResult::default())
        }
    });
    (registration, received)
}

async fn offered(root: &Conversation) -> Vec<String> {
    root.agent(&ctx()).await.unwrap().tools.iter().map(|tool| tool.name().to_owned()).collect()
}

async fn callable(root: &Conversation) -> Vec<String> {
    root.agent(&ctx()).await.unwrap().callable.iter().map(|tool| tool.name().to_owned()).collect()
}

fn setup_with(tools: Vec<ToolRegistration>) -> ChatSetup {
    let setup = chat_setup(Default::default());
    for tool in tools {
        add_tool(&setup, tool);
    }
    setup
}

async fn go(root: &Conversation) {
    root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
}

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|name| (*name).to_owned()).collect()
}

fn model_tools(filter: ToolFilter) -> AgentChange {
    AgentChange { model_tools: Change::Set(filter), ..AgentChange::default() }
}

fn first_code(result: Option<&NestedToolExecutionResult>) -> Option<String> {
    result.and_then(|result| result.diagnostics.first()).and_then(|diagnostic| diagnostic.code.clone())
}

#[tokio::test]
async fn offers_the_model_only_tools_it_may_call_and_lets_tools_call_only_tools_they_may_call() {
    let script = text_tool("script").with_callers(vec![ToolCaller::Model]);
    let hidden = text_tool("hidden").with_callers(vec![ToolCaller::Tools]);
    let (probe, received) = prober(&["script", "hidden", "both"]);
    let setup = setup_with(vec![script, hidden, text_tool("both"), probe]);
    setup.faux.set_responses([call("probe", "c1"), answer("done")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    assert_eq!(offered(&root).await, ["script", "both", "probe"]);
    assert_eq!(callable(&root).await, ["hidden", "both", "probe"]);
    go(&root).await;
    let received = received.lock().unwrap().clone();
    assert_eq!(first_code(received.get("script")).as_deref(), Some("tool_unavailable"));
    for name in ["hidden", "both"] {
        let result = &received[name];
        assert!(!result.is_error);
        assert_eq!(result.structured_output, Some(json!(format!("{name} ran")).into()));
    }
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn narrows_what_the_model_is_offered_with_model_tools_leaving_the_rest_callable_by_tools() {
    let (probe, received) = prober(&["read", "bash"]);
    let setup = setup_with(vec![text_tool("read"), text_tool("bash"), probe]);
    setup.faux.set_responses([call("probe", "c1"), answer("done")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    root.configure(model_tools(ToolFilter::List(names(&["probe"]))), &ctx()).await.unwrap();
    assert_eq!(offered(&root).await, ["probe"]);
    assert_eq!(callable(&root).await, ["read", "bash", "probe"]);
    go(&root).await;
    assert!(!received.lock().unwrap()["read"].is_error);
    assert!(!received.lock().unwrap()["bash"].is_error);

    root.configure(model_tools(ToolFilter::Remove { remove: names(&["bash"]) }), &ctx()).await.unwrap();
    assert_eq!(offered(&root).await, ["read", "probe"]);
    let agent = harness.snapshot(&AGENT_DOC, root.id, &ctx()).await.unwrap().unwrap();
    assert_eq!(agent.get("modelTools"), Some(&json!({ "remove": ["bash"] }).into()));
    let clear = AgentChange { model_tools: Change::Clear, ..AgentChange::default() };
    root.configure(clear, &ctx()).await.unwrap();
    assert_eq!(offered(&root).await, ["read", "bash", "probe"]);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn never_offers_a_tool_that_is_not_enabled_nor_lets_tools_call_it() {
    let (probe, received) = prober(&["bash"]);
    let setup = setup_with(vec![text_tool("read"), text_tool("bash"), probe]);
    setup.faux.set_responses([call("probe", "c1"), answer("done")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    // modelTools cannot bring back what tools disabled.
    let change = AgentChange {
        tools: Change::Set(ToolFilter::Remove { remove: names(&["bash"]) }),
        model_tools: Change::Set(ToolFilter::List(names(&["bash", "probe"]))),
        ..AgentChange::default()
    };
    root.configure(change, &ctx()).await.unwrap();
    assert_eq!(offered(&root).await, ["probe"]);
    assert_eq!(callable(&root).await, ["read", "probe"]);
    go(&root).await;
    assert_eq!(first_code(received.lock().unwrap().get("bash")).as_deref(), Some("tool_unavailable"));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn answers_a_model_issued_call_of_a_tool_it_was_not_offered_as_unavailable() {
    let setup = setup_with(vec![text_tool("read"), text_tool("bash")]);
    setup.faux.set_responses([call("bash", "c1"), answer("done")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    root.configure(model_tools(ToolFilter::List(names(&["read"]))), &ctx()).await.unwrap();
    go(&root).await;
    let entries = all_entries(&root).await;
    let result = entries.iter().find(|entry| entry.kind == "pi.tool-result").unwrap();
    assert!(serde_json::to_string(&result.model).unwrap().contains("Tool bash is not available"));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn applies_add_tools_to_model_tools_too_so_the_model_is_offered_the_added_tools() {
    let grow = define_tool("grow", "grow", no_parameters(), |_, _, _| async {
        let control = ToolControl { add_tools: Some(vec!["extra".into()]), ..ToolControl::default() };
        Ok(ToolExecutionResult { output: Some(Vec::new()), control: Some(control), ..ToolExecutionResult::default() })
    });
    let setup = setup_with(vec![text_tool("extra"), grow]);
    setup.faux.set_responses([call("grow", "c1"), answer("done")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    root.configure(model_tools(ToolFilter::List(names(&["grow"]))), &ctx()).await.unwrap();
    go(&root).await;
    let agent = harness.snapshot(&AGENT_DOC, root.id, &ctx()).await.unwrap().unwrap();
    assert_eq!(agent.get("modelTools"), Some(&json!(["grow", "extra"]).into()));
    assert_eq!(offered(&root).await, ["grow", "extra"]);
    harness.close(&ctx()).await.unwrap();
}
