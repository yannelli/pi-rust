//! Upstream `describe("tool rounds as the view shows them")`.

use std::sync::Arc;

use pi_durable::harness::*;
use pi_durable::session::Tx;
use pi_durable::storage::MemoryStorage;
use pi_durable::tasks::TaskOptions;
use pi_durable::*;
use serde_json::{Value, json};

use super::nodes::*;
use super::support::chat::{add_tool, answer, no_parameters};
use super::support::context;
use super::support::harness_support::{chat_setup, open_chat};
use super::support::live_support::{generations, graph_history, slots_of, view_history};
use super::support::task_support::eventually;
use super::tools::{blocking_tool, hook_after_tools, live, noop_tool, tasks_of, tool_calls};

/// The tool result entries of a view, by call ID, in transcript order.
fn results_of(view: &Value) -> Vec<String> {
    let entries = view["entries"].as_array().cloned().unwrap_or_default();
    let message = |entry: &Value| entry["model"][0].clone();
    let results = entries.iter().map(message).filter(|message| message["role"] == "toolResult");
    results.map(|message| message["toolCallId"].as_str().unwrap_or_default().to_owned()).collect()
}

#[tokio::test]
async fn ends_an_aborted_sequential_round_s_unstarted_calls_with_their_result_entries_right_before_them() {
    let setup = chat_setup(Default::default());
    let (one, one_started) = blocking_tool("one");
    add_tool(&setup, one);
    add_tool(&setup, blocking_tool("two").0);
    setup.faux.set_responses([tool_calls(&[("one", "c1"), ("two", "c2"), ("two", "c3")])]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let history = view_history(&harness, root.id).await;
    let submission = root.submit(SubmissionDraft::input("go"), &context()).await.unwrap();
    one_started.wait().await;
    let generation = live(&harness, &root).await.run.unwrap().task_id;
    harness.abort_task(generation, &context()).await.unwrap();
    submission.wait(&context()).await.unwrap();
    history.detach();
    // The started call ends with its result entry; the commit that ends the run appends the unstarted calls' results,
    // in call order, and removes the round.
    let (states, views) = (history.live(), history.views());
    let c1_done = slots_of(&states, "c1").into_iter().flatten().find(|slot| slot["status"] == "done").unwrap();
    let last = views.last().unwrap();
    assert_eq!(results_of(last), ["c1", "c2", "c3"]);
    let entries = last["entries"].as_array().unwrap();
    let c1_result = entries.iter().find(|entry| entry["model"][0]["toolCallId"] == "c1").unwrap();
    assert_eq!(c1_done["entry"], c1_result["id"]);
    let ending = views.iter().position(|view| results_of(view).contains(&"c2".to_owned())).unwrap();
    assert_eq!(results_of(&views[ending]), ["c1", "c2", "c3"]);
    assert!(states[ending].get("run").is_none() && states[ending].get("tools").is_none());
    let slots: Vec<Value> =
        states[ending - 1]["tools"].as_array().unwrap().iter().map(|s| json!([s["callId"], s["status"]])).collect();
    assert_eq!(slots, [json!(["c1", "done"]), json!(["c2", "pending"]), json!(["c3", "pending"])]);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn shows_a_held_generation_as_completing_to_an_observer_attached_while_it_holds() {
    let setup = chat_setup(Default::default());
    let nodes = Nodes::new();
    add_tool(&setup, noop_tool());
    let opened = hook_after_tools(&setup, &nodes);
    setup.faux.set_responses([tool_calls(&[("noop", "c1")]), answer("done")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    *opened.lock().unwrap() = Some(harness.clone());
    let early = graph_history(&harness).await;
    root.submit(SubmissionDraft::input("go"), &context()).await.unwrap().wait(&context()).await.unwrap();
    let late = graph_history(&harness).await;
    nodes.open_ok("hooked");
    let query = TaskQuery { kind: Some("pi.generation".into()), ..TaskQuery::default() };
    let first = tasks_of(&harness, query).await.remove(0);
    harness.wait_for_task(first.id, &context()).await.unwrap();
    // The early observer saw both generations; the late one starts with the held one, completing, and sees it leave.
    let seen = |graphs: &[Value]| {
        let ids = graphs.iter().flat_map(generations).filter_map(|node| node["id"].as_u64());
        ids.collect::<std::collections::BTreeSet<_>>().len()
    };
    assert_eq!(seen(&early.views()), 2);
    let held = generations(&late.views()[0]);
    assert_eq!(held.len(), 1);
    assert_eq!((&held[0]["id"], &held[0]["state"]["status"]), (&json!(first.id), &json!("completing")));
    assert!(late.views().last().unwrap()["tasks"].get(first.id.to_string()).is_none());
    early.detach();
    late.detach();
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn lets_a_tool_that_owns_live_work_finish_its_call_at_the_hold_while_the_generation_waits_for_its_final_commit() {
    let setup = chat_setup(Default::default());
    let nodes = Nodes::new();
    setup.registry.install(Extension::new("task:test.node").task(nodes.task.clone())).unwrap();
    let task = nodes.task.clone();
    let delegate =
        define_tool("delegate", "Starts work in a conversation it owns", no_parameters(), move |_, api, context| {
            let task = task.clone();
            async move {
                let owner = api.task_id;
                let start = move |tx: Tx| async move {
                    let child = tx.create_conversation(ConversationOwnership::Task { task_id: owner }).await?;
                    tx.create_task(task, named("sub"), TaskOptions::conversation(child.id)).await.map(|_| ())
                };
                api.commit(start, &context).await?;
                Ok(ToolExecutionResult::text("started"))
            }
        });
    add_tool(&setup, delegate);
    setup.faux.set_responses([tool_calls(&[("delegate", "c1")]), answer("done")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let submission = root.submit(SubmissionDraft::input("go"), &context()).await.unwrap();
    let done = || async {
        let slot = live(&harness, &root).await.tools.and_then(|tools| tools.first().cloned());
        slot.is_some_and(|slot| serde_json::to_value(slot.status).unwrap() == json!("done"))
    };
    eventually(done).await;
    let state_now = live(&harness, &root).await;
    let slot = state_now.tools.as_ref().unwrap()[0].clone();
    let tool = slot.task_id.unwrap();
    assert!(slot.entry.is_some());
    assert_eq!(status(&harness, tool).await, TaskStatus::Completing);
    assert_eq!(status(&harness, state_now.run.unwrap().task_id).await, TaskStatus::Waiting);
    let waiter = submission.clone();
    assert!(!settled(async move { waiter.wait(&context()).await }).await);
    nodes.open_ok("sub");
    assert_eq!(submission.wait(&context()).await.unwrap().status, SubmissionStatus::Done);
    assert_eq!(outcome_of(&harness, tool).await, "completed");
    harness.close(&context()).await.unwrap();
}

// Not ported: "holds a faulted tool's slot and its graph node until the work it owns drained". Its tool returns details
// that are not strict JSON to fault the task; a Rust result is always JSON.
