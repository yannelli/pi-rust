//! Upstream `describe("tool rounds and events")`.

use std::sync::{Arc, Mutex};

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
use super::support::task_support::eventually;
use super::tools::{blocking_tool, hook_after_tools, live, noop_tool, tasks_of, tool_calls};

type Events = Arc<Mutex<Vec<Value>>>;

async fn listen(harness: &Harness, conversation: &Conversation) -> (AgentEventStream, Events) {
    let stream = watch_events(harness, conversation.id, &context()).await.unwrap();
    let events = Events::default();
    let sink = events.clone();
    stream
        .start(move |pi_chord::NativeWatchChange { value: batch }, _| {
            sink.lock().unwrap().extend(batch.iter().map(Value::from));
            async { Ok(()) }
        })
        .unwrap();
    (stream, events)
}

/// Event types, with tool ends and message ends labelled by call ID and whether an entry came along.
fn labels(events: &Events) -> Vec<String> {
    let events = events.lock().unwrap();
    events
        .iter()
        .filter_map(|event| match event["type"].as_str()? {
            "tool_execution_end" => {
                Some(format!("end:{}:{}", event["toolCallId"].as_str()?, !event["entry"].is_null()))
            }
            "message_end" => {
                let message = &event["entry"]["model"][0];
                Some(match message["role"].as_str() {
                    Some("toolResult") => format!("result:{}", message["toolCallId"].as_str()?),
                    role => format!("message:{}", role.unwrap_or("undefined")),
                })
            }
            _ => None,
        })
        .collect()
}

fn count(events: &Events, kind: &str) -> usize {
    events.lock().unwrap().iter().filter(|event| event["type"] == kind).count()
}

#[tokio::test]
async fn ends_an_aborted_sequential_round_s_unstarted_calls_with_their_result_entries_right_before_them() {
    let setup = chat_setup(Default::default());
    let (one, one_started) = blocking_tool("one");
    add_tool(&setup, one);
    add_tool(&setup, blocking_tool("two").0);
    setup.faux.set_responses([tool_calls(&[("one", "c1"), ("two", "c2"), ("two", "c3")])]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let (stream, events) = listen(&harness, &root).await;
    let submission = root.submit(SubmissionDraft::input("go"), &context()).await.unwrap();
    one_started.wait().await;
    let generation = live(&harness, &root).await.run.unwrap().task_id;
    harness.abort_task(generation, &context()).await.unwrap();
    submission.wait(&context()).await.unwrap();
    eventually(|| async { labels(&events).contains(&"result:c3".to_owned()) }).await;
    let labels: Vec<String> = labels(&events).into_iter().filter(|label| !label.starts_with("message:")).collect();
    assert_eq!(labels, ["end:c1:true", "result:c1", "end:c2:true", "result:c2", "end:c3:true", "result:c3"]);
    stream.stop().await;
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn emits_one_turn_end_per_generation_also_for_a_stream_attached_while_it_holds() {
    let setup = chat_setup(Default::default());
    let nodes = Nodes::new();
    add_tool(&setup, noop_tool());
    let opened = hook_after_tools(&setup, &nodes);
    setup.faux.set_responses([tool_calls(&[("noop", "c1")]), answer("done")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    *opened.lock().unwrap() = Some(harness.clone());
    let (early_stream, early) = listen(&harness, &root).await;
    root.submit(SubmissionDraft::input("go"), &context()).await.unwrap().wait(&context()).await.unwrap();
    let (late_stream, late) = listen(&harness, &root).await;
    nodes.open_ok("hooked");
    let query = TaskQuery { kind: Some("pi.generation".into()), ..TaskQuery::default() };
    let first = tasks_of(&harness, query).await.remove(0);
    harness.wait_for_task(first.id, &context()).await.unwrap();
    let id = root.id;
    let note = move |tx: Tx| async move { tx.append_entry(id, EntryDraft::new("note")).await };
    root.commit(note, &context()).await.unwrap();
    eventually(|| async { count(&late, "entry_appended") > 0 }).await;
    assert_eq!((count(&early, "turn_end"), count(&late, "turn_end")), (2, 0));
    early_stream.stop().await;
    late_stream.stop().await;
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
    let (stream, events) = listen(&harness, &root).await;
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
    eventually(|| async { labels(&events).contains(&"end:c1:true".to_owned()) }).await;
    let waiter = submission.clone();
    assert!(!settled(async move { waiter.wait(&context()).await }).await);
    nodes.open_ok("sub");
    assert_eq!(submission.wait(&context()).await.unwrap().status, SubmissionStatus::Done);
    assert_eq!(outcome_of(&harness, tool).await, "completed");
    stream.stop().await;
    harness.close(&context()).await.unwrap();
}

// Not ported: "holds a faulted tool's slot and task_failed until the work it owns drained". Its tool returns details
// that are not strict JSON to fault the task; a Rust result is always JSON.
