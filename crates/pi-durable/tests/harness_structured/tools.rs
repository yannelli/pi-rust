//! Upstream `describe("tool rounds")`.

use std::sync::{Arc, Mutex};

use pi_ai::Message;
use pi_chord::Context;
use pi_durable::harness::generation::{GenerationHooks, generation_task};
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::harness::*;
use pi_durable::storage::MemoryStorage;
use pi_durable::tasks::TaskOptions;
use pi_durable::*;
use serde_json::json;

use super::nodes::*;
use super::support::chat::{add_tool, answer, calls, no_parameters};
use super::support::harness_support::{ChatSetup, all_entries, chat_setup, open_chat};
use super::support::live_support::{generations, graph_history, view_history};
use super::support::task_support::{aborted, eventually};
use super::support::{Deferred, context};

/// A sequential tool that writes partial output, then blocks until its call is cancelled; `started` fires first.
pub fn blocking_tool(name: &str) -> (ToolRegistration, Deferred) {
    let started = Deferred::default();
    let signal = started.clone();
    let tool = define_tool(name, format!("The {name} tool"), no_parameters(), move |_, api, context: Context| {
        let started = signal.clone();
        async move {
            api.output("partial")?;
            started.resolve();
            let cancel = context.abort_signal().unwrap().clone();
            aborted(cancel.clone()).await;
            Err(cancel.reason().map_or_else(DurableError::aborted, DurableError::from))
        }
    });
    (tool.with_execution_mode(ToolExecutionMode::Sequential), started)
}

pub fn noop_tool() -> ToolRegistration {
    define_tool("noop", "Does nothing", no_parameters(), |_, _, _| async {
        Ok(ToolExecutionResult { output: Some(Vec::new()), ..Default::default() })
    })
}

/// One assistant message calling each `(name, id)` without arguments.
pub fn tool_calls(list: &[(&str, &str)]) -> pi_durable::testing::faux::FauxResponseStep {
    calls(&list.iter().map(|(name, id)| (*name, json!({}), *id)).collect::<Vec<_>>())
}

pub async fn live(harness: &Harness, conversation: &Conversation) -> pi_durable::harness::live::LiveState {
    let value = harness.snapshot(&LIVE_DOC, conversation.id, &context()).await.unwrap();
    value.map(|value| value.deserialize_into().unwrap()).unwrap_or_default()
}

pub async fn tasks_of(harness: &Harness, query: TaskQuery) -> Vec<TaskRecord> {
    harness.commit(move |tx| async move { tx.scan_tasks(query, 20, None).await }, &context()).await.unwrap().items
}

pub fn tool_results(entries: &[EntryRecord]) -> Vec<pi_ai::ToolResultMessage> {
    let results = entries.iter().filter(|entry| entry.kind == "pi.tool-result");
    results
        .filter_map(|entry| match entry.model.as_ref()?.first()? {
            Message::ToolResult(result) => Some(result.clone()),
            _ => None,
        })
        .collect()
}

/// Install an `after_tools` hook that starts a `hooked` node owned by the generation, which holds it.
pub fn hook_after_tools(setup: &ChatSetup, nodes: &Nodes) -> Arc<Mutex<Option<Harness>>> {
    let opened = Arc::new(Mutex::new(None::<Harness>));
    let (slot, task) = (opened.clone(), nodes.task.clone());
    let hooks = GenerationHooks {
        after_tools: Some(Arc::new(move |_, api: HookApi, context: Context| {
            let (harness, task) = (slot.lock().unwrap().clone().unwrap(), task.clone());
            Box::pin(async move {
                let owner = api.task_id();
                let create = move |tx: pi_durable::session::Tx| async move {
                    tx.create_task(task, named("hooked"), TaskOptions::owned_by(owner)).await.map(|_| ())
                };
                harness.commit(create, &context).await
            })
        })),
        ..GenerationHooks::default()
    };
    setup.registry.install(Extension::new("hooks").hook(hook(&generation_task(), hooks))).unwrap();
    setup.registry.install(Extension::new("task:test.node").task(nodes.task.clone())).unwrap();
    opened
}

#[tokio::test]
async fn owns_its_tool_tasks_waits_for_them_and_hands_the_run_to_a_conversation_owned_generation() {
    let setup = chat_setup(Default::default());
    add_tool(&setup, noop_tool());
    setup.faux.set_responses([tool_calls(&[("noop", "c1"), ("noop", "c2")]), answer("done")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let settled = root.submit(SubmissionDraft::input("go"), &context()).await.unwrap().wait(&context()).await.unwrap();
    assert_eq!(settled.status, SubmissionStatus::Done);
    let tasks = tasks_of(&harness, TaskQuery { conversation_id: Some(root.id), ..TaskQuery::default() }).await;
    let generations: Vec<&TaskRecord> = tasks.iter().filter(|task| task.kind == "pi.generation").collect();
    let (first, second) = (generations[0], generations[1]);
    let owners: Vec<Option<TaskId>> =
        tasks.iter().filter(|task| task.kind == "pi.tool").map(|task| task.owner).collect();
    assert_eq!(owners, [Some(first.id), Some(first.id)]);
    assert_eq!((first.owner, second.owner), (None, None));
    let entries = all_entries(&root).await;
    let assistant = entries.iter().find(|entry| entry.kind == "pi.assistant").unwrap();
    let expected =
        json!({ "status": "terminal", "outcome": { "status": "completed", "result": { "entryId": assistant.id } } });
    assert_eq!(serde_json::to_value(&first.state).unwrap(), expected);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn aborts_a_parallel_round_with_its_generation_tools_first_then_the_generation_with_every_result_written() {
    let setup = chat_setup(Default::default());
    let (one, one_started) = blocking_tool("one");
    let (two, two_started) = blocking_tool("two");
    add_tool(&setup, one.with_execution_mode(ToolExecutionMode::Parallel));
    add_tool(&setup, two.with_execution_mode(ToolExecutionMode::Parallel));
    setup.faux.set_responses([tool_calls(&[("one", "c1"), ("two", "c2")])]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let submission = root.submit(SubmissionDraft::input("go"), &context()).await.unwrap();
    one_started.wait().await;
    two_started.wait().await;
    let generation = live(&harness, &root).await.run.unwrap().task_id;
    harness.abort_task(generation, &context()).await.unwrap();
    let record = submission.wait(&context()).await.unwrap();
    assert_eq!((record.status, record.reason.as_deref()), (SubmissionStatus::Unanswered, Some("aborted")));
    let ids: Vec<String> =
        tool_results(&all_entries(&root).await).into_iter().map(|result| result.tool_call_id).collect();
    assert_eq!(ids, ["c1", "c2"]);
    let failure = json!({ "taskId": generation, "inputs": [submission.id], "reason": "aborted" });
    assert_eq!(serde_json::to_value(live(&harness, &root).await).unwrap(), json!({ "runFailure": failure }));
    assert_eq!(outcome_of(&harness, generation).await, "aborted");
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn answers_the_unstarted_calls_of_an_aborted_sequential_round_with_aborted_results_in_call_order() {
    let setup = chat_setup(Default::default());
    let (one, one_started) = blocking_tool("one");
    add_tool(&setup, one);
    add_tool(&setup, blocking_tool("two").0);
    setup.faux.set_responses([tool_calls(&[("one", "c1"), ("two", "c2"), ("two", "c3")])]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let submission = root.submit(SubmissionDraft::input("go"), &context()).await.unwrap();
    one_started.wait().await;
    let slots = live(&harness, &root).await.tools.unwrap();
    assert_eq!(slots.iter().map(|slot| slot.task_id.is_some()).collect::<Vec<_>>(), [true, false, false]);
    let generation = live(&harness, &root).await.run.unwrap().task_id;
    harness.abort_task(generation, &context()).await.unwrap();
    let record = submission.wait(&context()).await.unwrap();
    assert_eq!((record.status, record.reason.as_deref()), (SubmissionStatus::Unanswered, Some("aborted")));
    let results = tool_results(&all_entries(&root).await);
    let ids: Vec<&str> = results.iter().map(|result| result.tool_call_id.as_str()).collect();
    assert_eq!(ids, ["c1", "c2", "c3"]);
    assert_eq!(results.iter().map(|result| result.is_error).collect::<Vec<_>>(), [true, true, true]);
    let text = "<harness>\n[error] Tool two was aborted\n</harness>";
    assert_eq!(serde_json::to_value(&results[1].content).unwrap(), json!([{ "type": "text", "text": text }]));
    let query = TaskQuery { conversation_id: Some(root.id), kind: Some("pi.tool".into()), ..TaskQuery::default() };
    assert_eq!(tasks_of(&harness, query).await.len(), 1);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn hands_run_control_to_the_successor_while_the_generation_holds_and_keeps_it_in_the_graph_until_it_ends() {
    let setup = chat_setup(Default::default());
    let nodes = Nodes::new();
    add_tool(&setup, noop_tool());
    // An extension's hook starts work owned by the generation, which holds it while the next turn runs.
    let opened = hook_after_tools(&setup, &nodes);
    setup.faux.set_responses([tool_calls(&[("noop", "c1")]), answer("done")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    *opened.lock().unwrap() = Some(harness.clone());
    let graph = graph_history(&harness).await;
    let history = view_history(&harness, root.id).await;
    root.submit(SubmissionDraft::input("go"), &context()).await.unwrap().wait(&context()).await.unwrap();
    let query = TaskQuery { kind: Some("pi.generation".into()), ..TaskQuery::default() };
    let first = tasks_of(&harness, query).await.remove(0);
    assert_eq!(status(&harness, first.id).await, TaskStatus::Completing);
    // The first generation's outcome is committed at its hold: the successor already runs the run, and the graph shows
    // the first one completing beside it.
    let handed = graph.views().into_iter().find(|value| generations(value).len() == 2).unwrap();
    let pair = generations(&handed);
    assert_eq!((&pair[0]["id"], &pair[0]["state"]["status"]), (&json!(first.id), &json!("completing")));
    assert_ne!(pair[1]["state"]["status"], "completing");
    assert!(history.live().iter().any(|live| live["run"]["taskId"] == pair[1]["id"]));
    nodes.open_ok("hooked");
    assert_eq!(outcome_of(&harness, first.id).await, "completed");
    eventually(|| async { graph.views().last().unwrap()["tasks"].get(first.id.to_string()).is_none() }).await;
    graph.detach();
    history.detach();
    harness.close(&context()).await.unwrap();
}

// Not ported: "keeps run control with a faulted generation until its owned work drains, and fails the Harness on a
// failed final commit". Its stream yields a final message that is not strict JSON; Rust messages are always JSON.
