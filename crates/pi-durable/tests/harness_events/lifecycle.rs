//! Run, abort, overflow, and close cases of upstream `test/harness-events.test.ts`.

use std::sync::Arc;

use pi_chord::{AbortController, ChordError, Json, with_abort_signal};
use pi_durable::harness::agent::AGENT_DOC;
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::harness::usage::USAGE_DOC;
use pi_durable::harness::*;
use pi_durable::session::{Tx, WatchEnd};
use pi_durable::storage::MemoryStorage;
use pi_durable::testing::faux::{FauxMessageOptions, RegisterFauxProviderOptions, faux_assistant_message, faux_text};
use pi_durable::{ConversationOwnership, DurableError, EntryDraft};
use serde_json::{Value, json};

use super::support::Deferred;
use super::support::harness_support::*;
use super::{add_tool, all, answer, apply_changes, calls, drained, listen, paced, types};

fn empty_object() -> Value {
    json!({ "type": "object", "properties": {} })
}

/// Edit the root's `pi.live` as JSON in one commit.
async fn change_live(root: &Conversation, edit: impl FnOnce(&mut Value) + Send + 'static) {
    let id = root.id;
    let commit = move |tx: Tx| async move {
        let draft = tx.doc(&LIVE_DOC, id).await?;
        let mut live = Value::from(&draft.value()?);
        edit(&mut live);
        draft.core().set(Json::from(live))
    };
    root.commit(commit, &ctx()).await.unwrap();
}

async fn append_note(root: &Conversation) {
    let id = root.id;
    root.commit(move |tx: Tx| async move { tx.append_entry(id, EntryDraft::new("note")).await.map(|_| ()) }, &ctx())
        .await
        .unwrap();
}

#[tokio::test]
async fn ends_a_call_that_never_runs_and_a_tool_aborted_with_its_generation() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    let wait =
        define_tool("wait", "Waits until aborted", empty_object(), |_, _, context: pi_chord::Context| async move {
            let error = context.abort_signal().unwrap().cancelled().await;
            Err(DurableError::from(error))
        });
    add_tool(&setup, wait);
    setup.faux.set_responses([calls(&[("ghost", json!({}), "c1"), ("wait", json!({}), "c2")])]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let (stream, batches) = listen(&harness, &root).await;
    let submission = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap();
    wait_for(|| {
        let started = all(&batches).iter().any(|event| event["type"] == "tool_execution_start");
        async move { started }
    })
    .await;
    // Aborting the generation aborts its round: the tool ends with its aborted result first (spec section 8.5).
    harness.abort_task(run_task(&harness, &root).await, &ctx()).await.unwrap();
    submission.wait(&ctx()).await.unwrap();
    drained().await;
    // The call not offered ends after its calling message and directly before its result message.
    let events = all(&batches);
    let round: Vec<String> = events
        .iter()
        .map(|event| match event["type"].as_str().unwrap() {
            "message_end" => format!("end:{}", event["entry"]["kind"].as_str().unwrap()),
            "message_start" => format!("start:{}", event["message"]["role"].as_str().unwrap()),
            other => other.to_owned(),
        })
        .collect();
    let ghost_end = round.iter().position(|kind| kind == "tool_execution_end").unwrap();
    assert_eq!(
        round[ghost_end - 1..ghost_end + 3],
        ["end:pi.assistant", "tool_execution_end", "start:toolResult", "end:pi.tool-result"]
    );
    let tool: Vec<(String, Value, bool)> = events
        .iter()
        .filter(|event| event["type"].as_str().unwrap().starts_with("tool_execution"))
        .map(|event| {
            (event["type"].as_str().unwrap().to_owned(), event["toolCallId"].clone(), event.get("entry").is_some())
        })
        .collect();
    let expected = [
        ("tool_execution_end".to_owned(), json!("c1"), true),
        ("tool_execution_start".to_owned(), json!("c2"), false),
        ("tool_execution_end".to_owned(), json!("c2"), true),
    ];
    assert_eq!(tool, expected);
    stream.stop().await;
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn reports_a_steer_without_run_events_a_reset_as_an_appended_entry_and_nothing_for_other_conversations() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    let gate = Deferred::default();
    let tool_gate = gate.clone();
    add_tool(
        &setup,
        define_tool("hold", "Waits", empty_object(), move |_, _, _| {
            let gate = tool_gate.clone();
            async move {
                gate.wait().await;
                Ok(ToolExecutionResult::default())
            }
        }),
    );
    setup.faux.set_responses([calls(&[("hold", json!({}), "c1")]), answer("done")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let ownerless = CreateOptions { ownership: ConversationOwnership::Ownerless, ..CreateOptions::default() };
    let other = harness.create_conversation(ownerless, &ctx()).await.unwrap();
    let (stream, batches) = listen(&harness, &root).await;
    let input = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    wait_for(|| {
        let started = all(&batches).iter().any(|event| event["type"] == "tool_execution_start");
        async move { started }
    })
    .await;
    root.submit(SubmissionDraft::input("s").with_when_busy(WhenBusy::Steer), &ctx()).await.unwrap();
    drained().await;
    let before = batches.lock().unwrap().len();
    append_note(&other).await;
    drained().await;
    assert_eq!(batches.lock().unwrap().len(), before);
    gate.resolve();
    input.wait(&ctx()).await.unwrap();
    root.reset(None, &ctx()).await.unwrap();
    drained().await;
    let kinds = types(&all(&batches));
    assert_eq!(kinds.iter().filter(|kind| *kind == "run_start").count(), 1);
    assert_eq!(kinds.iter().filter(|kind| *kind == "run_end").count(), 1);
    let last = batches.lock().unwrap().last().cloned().unwrap();
    assert_eq!(types(&last), ["entry_appended", "submission"]);
    stream.stop().await;
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn rejects_an_attachment_cancelled_while_it_waits_for_the_session_line() {
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &chat_setup(Default::default())).await;
    let release = Deferred::default();
    let gate = release.clone();
    let blocking_root = root.clone();
    let blocking = tokio::spawn(async move {
        blocking_root
            .commit(
                move |_| async move {
                    gate.wait().await;
                    Ok(())
                },
                &ctx(),
            )
            .await
    });
    super::drained().await;
    let controller = AbortController::new();
    let cancelled = with_abort_signal(&controller.signal(), &ctx());
    let (attaching_harness, id) = (harness.clone(), root.id);
    let attaching = tokio::spawn(async move { watch_events(&attaching_harness, id, &cancelled).await.map(|_| ()) });
    super::drained().await;
    controller.abort(Some(ChordError::error("cancelled")));
    release.resolve();
    blocking.await.unwrap().unwrap();
    let error = attaching.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("cancelled"), "{error}");
    harness.close(&ctx()).await.unwrap();
}

fn partial_message(text: &str) -> Value {
    let message = serde_json::to_value(faux_assistant_message(vec![faux_text(text)], FauxMessageOptions::default()));
    Value::from(&Json::from(message.unwrap()))
}

#[tokio::test]
async fn delivers_each_of_100_pending_batches_in_its_own_call_in_order_unlike_state_watches() {
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &chat_setup(Default::default())).await;
    let stream = watch_events(&harness, root.id, &ctx()).await.unwrap();
    for index in 0..100 {
        let id = root.id;
        let draft = EntryDraft { data: Some(Json::from(json!({ "index": index }))), ..EntryDraft::new("note") };
        let note = move |tx: Tx| async move { tx.append_entry(id, draft).await.map(|_| ()) };
        root.commit(note, &ctx()).await.unwrap();
    }
    let batches = super::Batches::default();
    let sink = batches.clone();
    stream
        .start(move |events, _| {
            sink.lock().unwrap().push(events.iter().map(Value::from).collect());
            async { Ok(()) }
        })
        .unwrap();
    drained().await;
    let batches = batches.lock().unwrap().clone();
    let data: Vec<Vec<Value>> =
        batches.iter().map(|events| events.iter().map(|event| event["entry"]["data"].clone()).collect()).collect();
    assert_eq!(data, (0..100).map(|index| vec![json!({ "index": index })]).collect::<Vec<_>>());
    stream.stop().await;
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn replaces_undelivered_batches_with_one_snapshot_after_100_pending_batches() {
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &chat_setup(Default::default())).await;
    let stream = watch_events(&harness, root.id, &ctx()).await.unwrap();
    for _ in 0..101 {
        append_note(&root).await;
    }
    let batches = super::Batches::default();
    let sink = batches.clone();
    stream
        .start(move |events, _| {
            sink.lock().unwrap().push(events.iter().map(Value::from).collect());
            async { Ok(()) }
        })
        .unwrap();
    drained().await;
    let batches = batches.lock().unwrap().clone();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].len(), 1);
    assert_eq!(batches[0][0]["type"], "snapshot");
    assert_eq!(batches[0][0]["entries"].as_array().unwrap().len(), 101);
    stream.stop().await;
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn applies_deltas_after_an_overflow_snapshot_that_holds_an_in_flight_partial() {
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &chat_setup(Default::default())).await;
    let partial = partial_message("hel");
    let stream = watch_events(&harness, root.id, &ctx()).await.unwrap();
    let message = partial.clone();
    change_live(&root, move |live| live["generation"] = json!({ "attempt": 1, "message": message })).await;
    for _ in 0..101 {
        append_note(&root).await;
    }
    let batches = super::Batches::default();
    let sink = batches.clone();
    stream
        .start(move |events, _| {
            sink.lock().unwrap().push(events.iter().map(Value::from).collect());
            async { Ok(()) }
        })
        .unwrap();
    drained().await;
    let snapshot = batches.lock().unwrap()[0][0].clone();
    assert_eq!(snapshot["type"], "snapshot");
    change_live(&root, |live| {
        let text = &mut live["generation"]["message"]["content"][0]["text"];
        *text = json!(format!("{}lo", text.as_str().unwrap()));
    })
    .await;
    drained().await;
    let update = batches.lock().unwrap().last().unwrap()[0].clone();
    assert_eq!(update["type"], "message_update");
    let changes = update["changes"].as_array().unwrap().clone();
    assert_eq!(changes, [json!({ "type": "text_delta", "contentIndex": 0, "delta": "lo" })]);
    let rebuilt = apply_changes(&snapshot["generation"]["message"], &changes);
    assert_eq!(rebuilt["content"], json!([{ "type": "text", "text": "hello" }]));
    stream.stop().await;
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn reports_usage_only_updates_cleared_tool_progress_tools_ending_without_entries_and_deferred_polls() {
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &chat_setup(Default::default())).await;
    let (stream, batches) = listen(&harness, &root).await;
    let partial = partial_message("partial");
    let message = partial.clone();
    change_live(&root, move |live| live["generation"] = json!({ "attempt": 1, "message": message })).await;
    // A usage-only change sends the usage and no changes.
    change_live(&root, |live| live["generation"]["message"]["usage"]["input"] = json!(42)).await;
    change_live(&root, |live| live["generation"] = json!({ "attempt": 1, "deferred": { "pollAt": 1 } })).await;
    change_live(&root, |live| live["generation"]["deferred"]["pollAt"] = json!(2)).await;
    let slot = json!([{ "callId": "c1", "name": "t", "status": "running", "details": { "n": 1 }, "diagnostics": [] }]);
    change_live(&root, move |live| live["tools"] = slot).await;
    // A safe replay clears the running slot's progress.
    change_live(&root, |live| {
        let slot = live["tools"][0].as_object_mut().unwrap();
        slot.remove("details");
        slot.remove("diagnostics");
    })
    .await;
    // A fault marks the slot done without an entry.
    change_live(&root, |live| live["tools"][0]["status"] = json!("done")).await;
    let id = root.id;
    root.commit(move |tx: Tx| async move { tx.retire_doc(&USAGE_DOC, id).await }, &ctx()).await.unwrap();
    root.commit(move |tx: Tx| async move { tx.retire_doc(&AGENT_DOC, id).await }, &ctx()).await.unwrap();
    drained().await;
    let batches: Vec<Vec<Value>> = batches
        .lock()
        .unwrap()
        .iter()
        .map(|batch| batch.iter().filter(|event| event["type"] != "task_failed").cloned().collect())
        .collect();
    let mut usage = partial["usage"].clone();
    usage["input"] = json!(42);
    let expected = vec![
        vec![json!({ "type": "message_start", "message": partial })],
        vec![json!({ "type": "message_update", "usage": usage, "changes": [] })],
        vec![json!({ "type": "deferred_poll", "pollAt": 1 })],
        vec![json!({ "type": "deferred_poll", "pollAt": 2 })],
        vec![json!({ "type": "tool_execution_start", "toolCallId": "c1", "toolName": "t", "args": {} })],
        vec![
            json!({ "type": "tool_execution_update", "toolCallId": "c1", "toolName": "t", "details": null, "diagnostics": [] }),
        ],
        vec![json!({ "type": "tool_execution_end", "toolCallId": "c1", "toolName": "t" })],
        // Retired documents read as their starting values.
        vec![json!({ "type": "usage_changed", "usage": { "models": {}, "tools": {} } })],
        vec![json!({ "type": "agent_changed", "agent": {} })],
    ];
    assert_eq!(batches, expected);
    stream.stop().await;
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn ends_the_stream_with_the_harness() {
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &chat_setup(Default::default())).await;
    let stream = watch_events(&harness, root.id, &ctx()).await.unwrap();
    harness.close(&ctx()).await.unwrap();
    assert!(matches!(stream.closed().await, WatchEnd::Ended));
}

#[tokio::test]
async fn starts_a_message_at_the_first_committed_partial_and_ends_it_with_the_converted_entry_on_abort() {
    let setup = paced(100.0);
    setup.faux.set_responses([answer(&"x".repeat(400))]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let (stream, batches) = listen(&harness, &root).await;
    let submission = root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap();
    let assistant_start = |event: &Value| event["type"] == "message_start" && event["message"]["role"] == "assistant";
    wait_for(|| {
        let started = all(&batches).iter().any(assistant_start);
        async move { started }
    })
    .await;
    harness.abort_task(run_task(&harness, &root).await, &ctx()).await.unwrap();
    submission.wait(&ctx()).await.unwrap();
    drained().await;
    let events = all(&batches);
    let ends = events.iter().filter(|event| event["type"] == "message_end" && event["entry"]["kind"] == "pi.assistant");
    assert_eq!(ends.count(), 1);
    assert_eq!(events.iter().filter(|event| assistant_start(event)).count(), 1);
    stream.stop().await;
    harness.close(&ctx()).await.unwrap();
}
