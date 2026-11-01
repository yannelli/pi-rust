//! Port of upstream `test/harness-view-run.test.ts`: what a renderer sees of a run through the conversation view, the
//! transcript window and `pi.live`, every change a commit makes, rebuilt from operations alone.

mod support;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::support::{ValueCall, on_value};
use pi_chord::Json;
use pi_chord::delta::{Op, apply_immutable};
use pi_durable::EntryDraft;
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::harness::*;
use pi_durable::session::Tx;
use pi_durable::storage::MemoryStorage;
use pi_durable::testing::faux::{FauxTokenSize, RegisterFauxProviderOptions};
use serde_json::{Value, json};
use support::Deferred;
use support::chat::{add_tool, answer, calls};
use support::harness_support::*;
use support::live_support::*;
use support::{change_record, change_value, document_changes, follow_commits};

#[path = "harness_view_run/partials.rs"]
mod partials;
#[path = "harness_view_run/runs.rs"]
mod runs;
#[path = "harness_view_run/tools.rs"]
mod tools;

type Frames = Arc<Mutex<Vec<(Json, Vec<Op>)>>>;

/// Record every frame `watch` delivers from now on.
fn start_recording(watch: &ConversationWatch) -> Frames {
    let frames: Frames = Arc::default();
    let sink = frames.clone();
    watch
        .start(on_value(move |ValueCall { value, ops, .. }, _| {
            sink.lock().unwrap().push((value, ops));
            async { Ok(()) }
        }))
        .unwrap();
    frames
}

/// Start a watch of `conversation` that records its acquisition value and every frame: what a renderer that keeps up
/// gets.
async fn record(conversation: &Conversation) -> (Json, Frames, ConversationWatch) {
    let watch = conversation.watch(&ctx(), None).await.unwrap();
    let acquired = watch.value();
    let frames = start_recording(&watch);
    (acquired, frames, watch)
}

/// Apply every frame's operations to the acquisition value, checking that each turns the value before it into its own.
fn replay(acquired: &Json, frames: &[(Json, Vec<Op>)]) -> Json {
    let mut value = acquired.clone();
    for (delivered, ops) in frames {
        value = apply_immutable(Some(&value), ops).unwrap();
        assert_eq!(&value, delivered);
    }
    value
}

fn ops_json(ops: &[Op]) -> Vec<Value> {
    ops.iter().map(|op| serde_json::to_value(op).unwrap()).collect()
}

fn live_of(view: &Json) -> Value {
    view.to_serde()["docs"].get("pi.live").cloned().unwrap_or_else(|| json!({}))
}

async fn drained() {
    tokio::time::sleep(Duration::from_millis(20)).await;
}

fn paced(tokens_per_second: f64) -> ChatSetup {
    let token_size = Some(FauxTokenSize { min: Some(1), max: Some(1) });
    chat_setup(RegisterFauxProviderOptions {
        tokens_per_second: Some(tokens_per_second),
        token_size,
        ..Default::default()
    })
}

type Committed = Arc<Mutex<Vec<Value>>>;

/// Committed values of `pi.live` that `pick` finds something in, one per commit that has one.
fn committed_live(harness: &Harness, pick: impl Fn(&Value) -> Option<Value> + Send + Sync + 'static) -> Committed {
    let values = Committed::default();
    let sink = values.clone();
    let unsubscribe = follow_commits(harness, move |publication, _| {
        for change in document_changes(publication) {
            let picked = change_value(&change).and_then(|value| pick(&value.to_serde()));
            if change_record(&change).kind == "pi.live"
                && let Some(picked) = picked
            {
                sink.lock().unwrap().push(picked);
            }
        }
        Ok(())
    })
    .unwrap();
    std::mem::forget(unsubscribe);
    values
}

/// Committed partials of the Harness's generations, one per commit that has one.
fn partials_of(harness: &Harness) -> Committed {
    committed_live(harness, |live| live["generation"].get("message").cloned())
}

/// The distinct in-flight partials in `states`, in order.
fn partials_in(states: &[Value]) -> Vec<Value> {
    let mut partials: Vec<Value> = Vec::new();
    for message in states.iter().filter_map(|state| state["generation"].get("message")) {
        if partials.last() != Some(message) {
            partials.push(message.clone());
        }
    }
    partials
}

/// Text of a message: its string content, or its first text block.
fn text_in(message: &Value) -> Option<String> {
    let content = &message["content"];
    if let Some(text) = content.as_str() {
        return Some(text.to_owned());
    }
    content.as_array()?.iter().find(|block| block["type"] == "text")?["text"].as_str().map(str::to_owned)
}

fn view_entries(view: &Value) -> &[Value] {
    view["entries"].as_array().map(Vec::as_slice).unwrap_or_default()
}

fn last_entry(view: &Value) -> &Value {
    view_entries(view).last().unwrap()
}

/// Edit `conversation`'s `pi.live` as JSON in one commit.
async fn change_live(conversation: &Conversation, edit: impl FnOnce(&mut Value) + Send + 'static) {
    let id = conversation.id;
    let commit = move |tx: Tx| async move {
        let draft = tx.doc(&LIVE_DOC, id).await?;
        let mut live = draft.value()?.to_serde();
        edit(&mut live);
        draft.core().set(Json::from(live))
    };
    conversation.commit(commit, &ctx()).await.unwrap();
}

async fn append_note(conversation: &Conversation) {
    let id = conversation.id;
    let note = move |tx: Tx| async move { tx.append_entry(id, EntryDraft::new("note")).await.map(|_| ()) };
    conversation.commit(note, &ctx()).await.unwrap();
}

#[tokio::test]
async fn streams_the_answer_as_operations_that_rebuild_every_committed_partial_then_appends_it() {
    let setup = paced(400.0);
    let text = "streamed answer text ".repeat(20);
    setup.faux.set_responses([answer(&text)]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let partials = partials_of(&harness);
    let history = view_history(&harness, root.id).await;
    let (acquired, frames, watch) = record(&root).await;
    let submission = root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap();
    submission.wait(&ctx()).await.unwrap();
    drained().await;
    history.detach();
    replay(&acquired, &frames.lock().unwrap());
    let (states, views) = (history.live(), history.views());
    let partials = partials.lock().unwrap().clone();
    assert!(partials.len() > 1, "{}", partials.len());
    let texts = |messages: &[Value]| messages.iter().map(text_in).collect::<Vec<_>>();
    assert_eq!(texts(&partials_in(&states)), texts(&partials));
    // The run shows from the input's commit until the commit that appends the answer and clears the partial.
    let started = states.iter().position(|state| state.get("run").is_some()).unwrap();
    let run = &states[started]["run"];
    assert!(run["taskId"].is_u64(), "{run}");
    assert_eq!(run, &json!({ "taskId": run["taskId"], "inputs": [submission.id] }));
    assert_eq!(last_entry(&views[started])["kind"], "pi.user");
    let ended = (started + 1..states.len()).find(|&index| states[index].get("run").is_none()).unwrap();
    assert!(states[ended].get("generation").is_none(), "{}", states[ended]);
    assert_eq!(text_in(&last_entry(&views[ended])["model"][0]), Some(text));
    assert_eq!(states.last(), Some(&json!({})));
    watch.stop().await;
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn shows_a_tool_calls_slot_from_pending_through_its_running_output_to_its_result_entry() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    let gate = Deferred::default();
    let tool_gate = gate.clone();
    let parameters = json!({ "type": "object", "properties": { "n": { "type": "number" } } });
    add_tool(
        &setup,
        define_tool("print", "Prints", parameters, move |_, api, _| {
            let gate = tool_gate.clone();
            async move {
                api.output("one\n")?;
                tokio::time::sleep(Duration::from_millis(150)).await;
                api.output("two\n")?;
                gate.wait().await;
                Ok(ToolExecutionResult::default())
            }
        }),
    );
    setup.faux.set_responses([calls(&[("print", json!({ "n": 1 }), "c1")]), answer("done")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let history = view_history(&harness, root.id).await;
    let submission = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap();
    wait_for(|| {
        let last = slots_of(&history.live(), "c1").pop().flatten();
        let done = last.is_some_and(|slot| slot["output"] == "one\ntwo\n");
        async move { done }
    })
    .await;
    gate.resolve();
    submission.wait(&ctx()).await.unwrap();
    history.detach();
    let (states, views) = (history.live(), history.views());
    let slots = slots_of(&states, "c1");
    let listed = slots.iter().position(Option::is_some).unwrap();
    // Listed pending with the tool-calling answer, in the same commit.
    let slot = slots[listed].clone().unwrap();
    assert!(slot["taskId"].is_u64(), "{slot}");
    assert_eq!(slot, json!({ "callId": "c1", "name": "print", "status": "pending", "taskId": slot["taskId"] }));
    assert_eq!(last_entry(&views[listed])["kind"], "pi.assistant");
    let mut outputs: Vec<String> = Vec::new();
    for slot in slots.iter().flatten().filter(|slot| slot["status"] == "running") {
        let output = slot["output"].as_str().unwrap_or_default().to_owned();
        if !outputs.contains(&output) {
            outputs.push(output);
        }
    }
    assert_eq!(outputs, ["", "one\n", "one\ntwo\n"]);
    // Done with its result entry, appended in the same commit.
    let done = slots.iter().position(|slot| slot.as_ref().is_some_and(|slot| slot["status"] == "done")).unwrap();
    let (slot, entry) = (slots[done].clone().unwrap(), last_entry(&views[done]));
    let expected =
        json!({ "callId": "c1", "name": "print", "status": "done", "taskId": slot["taskId"], "entry": entry["id"] });
    assert_eq!(slot, expected);
    let result = &entry["model"][0];
    assert_eq!((&result["role"], &result["toolCallId"]), (&json!("toolResult"), &json!("c1")));
    // The run's two generations, the tool round and the answer, hand the run over.
    let run_tasks: BTreeSet<u64> = states.iter().filter_map(|state| state["run"]["taskId"].as_u64()).collect();
    assert_eq!(run_tasks.len(), 2, "{run_tasks:?}");
    harness.close(&ctx()).await.unwrap();
}
