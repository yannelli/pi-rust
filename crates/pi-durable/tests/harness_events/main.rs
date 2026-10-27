//! Port of upstream `test/harness-events.test.ts`. This file holds the helpers and the streaming cases; `lifecycle.rs`
//! holds the run, abort, overflow, and close cases.

#[path = "../support/mod.rs"]
mod support;

mod lifecycle;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_ai::StopReason;
use pi_durable::harness::*;
use pi_durable::storage::MemoryStorage;
use pi_durable::testing::faux::{
    FauxMessageOptions, FauxResponseStep, FauxTokenSize, RegisterFauxProviderOptions, faux_assistant_message,
    faux_text, faux_thinking, faux_tool_call_with_id,
};
use serde_json::{Value, json};
use support::harness_support::*;
use support::{change_record, change_value, document_changes, observe_commits};

pub type Batches = Arc<Mutex<Vec<Vec<Value>>>>;

/// Attach and start an event stream that records every delivered batch.
pub async fn listen(harness: &Harness, conversation: &Conversation) -> (AgentEventStream, Batches) {
    let stream = watch_events(harness, conversation.id, &ctx()).await.unwrap();
    let batches = Batches::default();
    let sink = batches.clone();
    stream
        .start(move |pi_chord::NativeWatchChange { value: events }, _| {
            sink.lock().unwrap().push(events.iter().map(Value::from).collect());
            async { Ok(()) }
        })
        .unwrap();
    (stream, batches)
}

pub fn all(batches: &Batches) -> Vec<Value> {
    batches.lock().unwrap().concat()
}

pub fn types(events: &[Value]) -> Vec<String> {
    events.iter().map(|event| event["type"].as_str().unwrap().to_owned()).collect()
}

pub async fn drained() {
    tokio::time::sleep(Duration::from_millis(20)).await;
}

pub fn paced(tokens_per_second: f64) -> ChatSetup {
    let token_size = Some(FauxTokenSize { min: Some(1), max: Some(1) });
    chat_setup(RegisterFauxProviderOptions {
        tokens_per_second: Some(tokens_per_second),
        token_size,
        ..Default::default()
    })
}

pub fn answer(text: &str) -> FauxResponseStep {
    faux_assistant_message(vec![faux_text(text)], FauxMessageOptions::default()).into()
}

pub fn calls(list: &[(&str, Value, &str)]) -> FauxResponseStep {
    let blocks =
        list.iter().map(|(name, args, id)| faux_tool_call_with_id(*name, args.clone(), *id)).collect::<Vec<_>>();
    faux_assistant_message(blocks, FauxMessageOptions::stop_reason(StopReason::ToolUse)).into()
}

pub fn add_tool(setup: &ChatSetup, tool: ToolRegistration) {
    let name = format!("tool:{}", tool.name());
    setup.registry.install(Extension::new(name).tool(tool)).unwrap();
}

/// A faux answer held until `release` resolves or its request is cancelled.
pub fn held(release: support::Deferred, text: &'static str) -> FauxResponseStep {
    FauxResponseStep::async_factory(move |_, options, _, _| {
        let (release, signal) = (release.clone(), options.signal.clone());
        Box::pin(async move {
            tokio::select! {
                () = release.wait() => Ok(faux_assistant_message(vec![faux_text(text)], FauxMessageOptions::default())),
                error = async { signal.unwrap().cancelled().await } => Err(error.to_string()),
            }
        })
    })
}

/// Rebuild the streamed text of block 0 from `message_start` and the text deltas that follow it.
fn streamed_text(events: &[Value]) -> String {
    let mut text = String::new();
    for event in events {
        if event["type"] == "message_start" && event["message"]["role"] == "assistant" {
            text = event["message"]["content"][0]["text"].as_str().unwrap_or_default().to_owned();
        }
        if event["type"] != "message_update" {
            continue;
        }
        for change in event["changes"].as_array().unwrap() {
            if change["type"] == "text_start" && change["contentIndex"] == 0 {
                text = change["block"]["text"].as_str().unwrap_or_default().to_owned();
            }
            if change["type"] == "text_delta" && change["contentIndex"] == 0 {
                text.push_str(change["delta"].as_str().unwrap());
            }
        }
    }
    text
}

/// Apply message changes to a copy of `message`, as an events-only consumer would.
pub fn apply_changes(message: &Value, changes: &[Value]) -> Value {
    let mut next = message.clone();
    for change in changes {
        let kind = change["type"].as_str().unwrap();
        let index = change["contentIndex"].as_u64().unwrap_or_default() as usize;
        let append = |target: &mut Value, delta: &Value| {
            *target = Value::from(format!("{}{}", target.as_str().unwrap_or_default(), delta.as_str().unwrap()));
        };
        match kind {
            "message" => next = change["message"].clone(),
            "block" => next["content"][index] = change["block"].clone(),
            "text_delta" => append(&mut next["content"][index]["text"], &change["delta"]),
            "thinking_delta" => append(&mut next["content"][index]["thinking"], &change["delta"]),
            "toolcall_delta" => {
                let mut target = &mut next["content"][index]["arguments"];
                for segment in change["path"].as_array().unwrap() {
                    target = match segment {
                        Value::String(key) => &mut target[key.as_str()],
                        other => &mut target[other.as_u64().unwrap() as usize],
                    };
                }
                append(target, &change["delta"]);
            }
            _ if kind.ends_with("_start") => {
                next["content"].as_array_mut().unwrap().insert(index, change["block"].clone());
            }
            _ => {}
        }
    }
    next
}

/// Committed values of `pi.live` that pass `pick`, one per commit that has one.
pub fn live_values(harness: &Harness, pick: impl Fn(&Value) -> Option<Value> + Send + Sync + 'static) -> Batches {
    let values = Batches::default();
    let sink = values.clone();
    let unsubscribe = observe_commits(harness, move |publication, _| {
        for change in document_changes(publication) {
            let Some(value) = change_value(&change) else { continue };
            if change_record(&change).kind == "pi.live"
                && let Some(picked) = pick(&Value::from(&value))
            {
                sink.lock().unwrap().push(vec![picked]);
            }
        }
        Ok(())
    });
    std::mem::forget(unsubscribe);
    values
}

fn partial(live: &Value) -> Option<Value> {
    live.get("generation")?.get("message").cloned()
}

#[tokio::test]
async fn streams_a_run_as_lifecycle_events_and_text_deltas_that_rebuild_the_answer() {
    let setup = paced(400.0);
    let text = "streamed answer text ".repeat(20);
    setup.faux.set_responses([answer(&text)]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let (stream, batches) = listen(&harness, &root).await;
    let snapshot = Value::from(&stream.snapshot);
    assert_eq!((snapshot["type"].clone(), snapshot["entries"].clone()), (json!("snapshot"), json!([])));
    assert_eq!((snapshot["tools"].clone(), snapshot["inbox"].clone()), (json!([]), json!([])));
    let partials = live_values(&harness, partial);
    let submission = root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap();
    submission.wait(&ctx()).await.unwrap();
    drained().await;
    let events = all(&batches);
    let mut kinds = types(&events);
    kinds.dedup();
    let expected = [
        "message_start",
        "message_end",
        "submission",
        "run_start",
        "turn_start",
        "message_start",
        "message_update",
        "message_end",
        "turn_end",
        "run_end",
        "submission",
        "usage_changed",
    ];
    assert_eq!(kinds, expected);
    let run_start = events.iter().find(|event| event["type"] == "run_start").unwrap();
    assert_eq!(run_start, &json!({ "type": "run_start", "inputs": [submission.id] }));
    // After each event, the rebuilt text equals the committed partial of that commit.
    let rebuilt: Vec<String> = (0..events.len())
        .filter(|&index| {
            let event = &events[index];
            (event["type"] == "message_start" && event["message"]["role"] == "assistant")
                || event["type"] == "message_update"
        })
        .map(|index| streamed_text(&events[..=index]))
        .collect();
    let partials: Vec<String> = all(&partials)
        .iter()
        .map(|message| message["content"][0]["text"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(rebuilt, partials);
    assert!(partials.len() > 1);
    let end = events.iter().rfind(|event| event["type"] == "message_end").unwrap();
    assert_eq!(end["entry"]["model"][0]["content"][0]["text"], json!(text));
    stream.stop().await;
    harness.close(&ctx()).await.unwrap();
}

/// The kept output window rebuilt from the output updates, after each one.
fn outputs(events: &[Value]) -> Vec<String> {
    let mut text = String::new();
    let mut rebuilt = Vec::new();
    for event in events.iter().filter(|event| event["type"] == "tool_execution_update") {
        let Some(output) = event.get("output") else { continue };
        if let Some(set) = output.get("set") {
            text = set.as_str().unwrap().to_owned();
        } else {
            // `trimStart` counts UTF-16 code units; the test output is ASCII.
            let trim = output["trimStart"].as_u64().unwrap_or_default() as usize;
            text = format!("{}{}", &text[trim..], output["append"].as_str().unwrap_or_default());
        }
        rebuilt.push(text.clone());
    }
    rebuilt
}

#[tokio::test]
async fn reports_tool_start_output_appends_and_the_result_entry() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    let gate = support::Deferred::default();
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
    let (stream, batches) = listen(&harness, &root).await;
    let submission = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap();
    // The output events rebuild the slot's kept window.
    wait_for(|| {
        let done = outputs(&all(&batches)).last().is_some_and(|text| text == "one\ntwo\n");
        async move { done }
    })
    .await;
    gate.resolve();
    submission.wait(&ctx()).await.unwrap();
    drained().await;
    let events = all(&batches);
    let tool: Vec<&Value> =
        events.iter().filter(|event| event["type"].as_str().unwrap().starts_with("tool_execution")).collect();
    assert!(tool[0]["taskId"].is_u64());
    let mut start = tool[0].clone();
    start.as_object_mut().unwrap().remove("taskId");
    assert_eq!(
        start,
        json!({ "type": "tool_execution_start", "toolCallId": "c1", "toolName": "print", "args": { "n": 1 } })
    );
    let end = *tool.last().unwrap();
    assert_eq!((end["type"].clone(), end["toolCallId"].clone()), (json!("tool_execution_end"), json!("c1")));
    assert_eq!(end["entry"]["kind"], "pi.tool-result");
    // As in the coding agent, the tool ends directly before its result message.
    let at = events.iter().position(|event| event == end).unwrap();
    assert_eq!(types(&events[at..at + 3]), ["tool_execution_end", "message_start", "message_end"]);
    let result = &events[at + 1]["message"];
    assert_eq!((result["role"].clone(), result["toolCallId"].clone()), (json!("toolResult"), json!("c1")));
    // Two turns: the tool round, and the answer.
    let kinds = types(&events);
    assert_eq!(kinds.iter().filter(|kind| *kind == "turn_start").count(), 2);
    assert_eq!(kinds.iter().filter(|kind| *kind == "turn_end").count(), 2);
    stream.stop().await;
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn reports_queued_submissions_inbox_changes_and_retries() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    let release = support::Deferred::default();
    let options = FauxMessageOptions {
        error_message: Some("503 Service Unavailable".into()),
        ..FauxMessageOptions::stop_reason(StopReason::Error)
    };
    let error = faux_assistant_message(Vec::new(), options);
    setup.faux.set_responses([held(release.clone(), "first"), error.into(), answer("second")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    setup.settings.lock().unwrap().retry = Some(RetryOverrides {
        enabled: Some(true),
        max_retries: Some(1),
        base_delay_ms: Some(1),
        ..Default::default()
    });
    let (stream, batches) = listen(&harness, &root).await;
    root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    let follow_up = root.submit(SubmissionDraft::input("f"), &ctx()).await.unwrap();
    drained().await;
    let inbox: Vec<Value> = all(&batches).into_iter().filter(|event| event["type"] == "inbox_update").collect();
    assert_eq!(inbox, [json!({ "type": "inbox_update", "items": [{ "id": follow_up.id, "mode": "followUp" }] })]);
    release.resolve();
    follow_up.wait(&ctx()).await.unwrap();
    drained().await;
    let events = all(&batches);
    let kinds = types(&events);
    assert!(kinds.contains(&"auto_retry_start".to_owned()));
    assert!(kinds.contains(&"auto_retry_end".to_owned()));
    assert_eq!(kinds.iter().filter(|kind| *kind == "run_start").count(), 2);
    let statuses: Vec<Value> = events
        .iter()
        .filter(|event| event["type"] == "submission")
        .map(|event| event["record"]["status"].clone())
        .collect();
    assert_eq!(statuses, [json!("placed"), json!("queued"), json!("done"), json!("placed"), json!("done")]);
    stream.stop().await;
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn rebuilds_every_committed_partial_of_thinking_text_and_tool_call_arguments_from_message_changes() {
    let setup = paced(150.0);
    let blocks = vec![
        faux_thinking("thinking about it ".repeat(10)),
        faux_text("some text ".repeat(10)),
        faux_tool_call_with_id("missing", json!({ "path": "a/long/path/".repeat(10), "note": "x".repeat(60) }), "c1"),
    ];
    let message = faux_assistant_message(blocks, FauxMessageOptions::stop_reason(StopReason::ToolUse));
    setup.faux.set_responses([message.into(), answer("done")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let partials = live_values(&harness, partial);
    let (stream, batches) = listen(&harness, &root).await;
    root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    drained().await;
    let mut rebuilt = Vec::new();
    let mut current = Value::Null;
    // The streamed tool-calling message, up to its end; the short final answer commits no partial.
    for event in all(&batches) {
        if event["type"] == "message_end" && event["entry"]["kind"] == "pi.assistant" {
            break;
        }
        if event["type"] == "message_start" && event["message"]["role"] == "assistant" {
            current = event["message"].clone();
        } else if event["type"] == "message_update" {
            current = apply_changes(&current, event["changes"].as_array().unwrap());
        } else {
            continue;
        }
        rebuilt.push(current["content"].clone());
    }
    let partials: Vec<Value> = all(&partials).iter().map(|message| message["content"].clone()).collect();
    assert!(partials.len() > 2);
    assert_eq!(rebuilt, partials);
    let changes: Vec<String> = all(&batches)
        .iter()
        .filter(|event| event["type"] == "message_update")
        .flat_map(|event| types(event["changes"].as_array().unwrap()))
        .collect();
    assert!(changes.iter().any(|kind| kind == "thinking_delta" || kind == "text_delta"));
    stream.stop().await;
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn rebuilds_a_sliding_tail_window_from_output_trims_and_appends() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    let gate = support::Deferred::default();
    let tool_gate = gate.clone();
    let tool = define_tool("tail", "Prints lines", json!({ "type": "object", "properties": {} }), move |_, api, _| {
        let gate = tool_gate.clone();
        async move {
            for line in 0..6 {
                api.output(&format!("line {line}\n"))?;
                tokio::time::sleep(Duration::from_millis(120)).await;
            }
            gate.wait().await;
            Ok(ToolExecutionResult::default())
        }
    })
    .with_output_limits(ToolOutputLimits { max_lines: Some(3), retain: Some(Retain::Tail), ..Default::default() });
    add_tool(&setup, tool);
    setup.faux.set_responses([calls(&[("tail", json!({}), "c1")]), answer("done")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let committed = live_values(&harness, |live| live.get("tools")?.get(0)?.get("output").cloned());
    let (stream, batches) = listen(&harness, &root).await;
    let submission = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap();
    wait_for(|| {
        let done = all(&committed).last().is_some_and(|output| output.as_str().unwrap().ends_with("line 5\n"));
        async move { done }
    })
    .await;
    gate.resolve();
    submission.wait(&ctx()).await.unwrap();
    drained().await;
    let mut expected: Vec<String> = all(&committed).iter().map(|output| output.as_str().unwrap().to_owned()).collect();
    expected.dedup();
    let events = all(&batches);
    assert_eq!(outputs(&events), expected);
    let trimmed =
        |event: &Value| event["type"] == "tool_execution_update" && event["output"].get("trimStart").is_some();
    assert!(events.iter().any(trimmed));
    stream.stop().await;
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn emits_one_exact_batch_when_a_run_ends_and_a_queued_follow_up_starts_the_next() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    let release = support::Deferred::default();
    setup.faux.set_responses([held(release.clone(), "first"), answer("second")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let input = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    let follow_up = root.submit(SubmissionDraft::input("f"), &ctx()).await.unwrap();
    let (stream, batches) = listen(&harness, &root).await;
    release.resolve();
    follow_up.wait(&ctx()).await.unwrap();
    drained().await;
    let batches = batches.lock().unwrap().clone();
    let boundary = batches.iter().find(|batch| batch.iter().any(|event| event["type"] == "run_end")).unwrap();
    let expected = [
        "message_start",
        "message_end",
        "message_start",
        "message_end",
        "turn_end",
        "run_end",
        "submission",
        "submission",
        "inbox_update",
        "usage_changed",
        "run_start",
        "turn_start",
    ];
    assert_eq!(types(boundary), expected);
    let ids: Vec<Value> = boundary
        .iter()
        .filter(|event| event["type"] == "submission")
        .map(|event| event["record"]["id"].clone())
        .collect();
    assert_eq!(ids, [json!(input.id), json!(follow_up.id)]);
    stream.stop().await;
    harness.close(&ctx()).await.unwrap();
}
