//! Port of upstream `test/harness-inbox.test.ts`. The modules under `harness_inbox/` hold the remaining boundary,
//! continuation, storage, and usage cases.

#[path = "harness_inbox/boundaries.rs"]
mod boundaries;
#[path = "harness_inbox/continuations.rs"]
mod continuations;
#[path = "harness_inbox/failed_run.rs"]
mod failed_run;
#[path = "harness_inbox/ledger.rs"]
mod ledger;
mod support;

use std::sync::{Arc, Mutex};

use pi_ai::{Cost, Message, StopReason, Usage};
use pi_chord::Json;
use pi_durable::harness::inbox::INBOX_DOC;
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::harness::usage::USAGE_DOC;
use pi_durable::harness::*;
use pi_durable::storage::MemoryStorage;
use pi_durable::testing::faux::{
    FauxMessageOptions, FauxResponseStep, RegisterFauxProviderOptions, faux_assistant_message, faux_text,
    faux_tool_call_with_id,
};
use pi_durable::{EntryDraft, EntryRecord, SubmissionId, SubmissionStatus};
use serde_json::json;
use support::harness_support::*;
use support::{Deferred, context};
use tokio::sync::Notify;

fn answer(text: &str) -> FauxResponseStep {
    faux_assistant_message(vec![faux_text(text)], FauxMessageOptions::default()).into()
}

/// A faux response held until released or cancelled; `reached` fires when the request is sent.
fn gated(text: &str) -> (FauxResponseStep, Arc<Notify>, Deferred) {
    let (reached, gate) = (Arc::new(Notify::new()), Deferred::default());
    let (notify, held, message) = (reached.clone(), gate.clone(), text.to_owned());
    let step = FauxResponseStep::async_factory(move |_, options, _, _| {
        let (notify, held, message, signal) = (notify.clone(), held.clone(), message.clone(), options.signal.clone());
        Box::pin(async move {
            notify.notify_one();
            tokio::select! {
                () = held.wait() => Ok(faux_assistant_message(vec![faux_text(message)], FauxMessageOptions::default())),
                reason = async { signal.unwrap().cancelled().await } => Err(reason.to_string()),
            }
        })
    });
    (step, reached, gate)
}

/// Register a `hold` tool whose calls wait for `gate` and then return `result`.
fn hold_tool(setup: &ChatSetup, gate: Deferred, result: ToolExecutionResult) {
    let tool = define_tool("hold", "Waits for the test", json!({ "type": "object" }), move |_, _, _| {
        let (gate, result) = (gate.clone(), result.clone());
        async move {
            gate.wait().await;
            Ok(result)
        }
    });
    setup.registry.install(Extension::new("tool:hold").tool(tool)).unwrap();
}

fn hold_call() -> FauxResponseStep {
    let call = faux_tool_call_with_id("hold", json!({}), "c1");
    faux_assistant_message(vec![call], FauxMessageOptions::stop_reason(StopReason::ToolUse)).into()
}

fn empty_result() -> ToolExecutionResult {
    ToolExecutionResult { output: Some(Vec::new()), ..ToolExecutionResult::default() }
}

/// Kind and text of each entry, skipping system entries.
fn transcript(entries: &[EntryRecord]) -> Vec<String> {
    entries
        .iter()
        .filter(|entry| entry.kind != "pi.system")
        .map(|entry| {
            let message = entry.model.as_ref().and_then(|model| model.first());
            let text = match message {
                Some(Message::ToolResult(_)) => None,
                other => text_of(other).filter(|text| !text.is_empty()),
            };
            match text {
                Some(text) => format!("{}:{text}", entry.kind),
                None => entry.kind.clone(),
            }
        })
        .collect()
}

async fn inbox(harness: &Harness, root: &Conversation) -> Vec<(SubmissionId, String)> {
    let state = harness.snapshot(&INBOX_DOC, root.id, &context()).await.unwrap().unwrap().to_serde();
    let items = state["items"].as_array().unwrap().clone();
    items
        .iter()
        .map(|item| (serde_json::from_value(item["id"].clone()).unwrap(), item["mode"].as_str().unwrap().to_owned()))
        .collect()
}

async fn tool_running(harness: &Harness, root: &Conversation) {
    wait_for(|| {
        let (harness, root) = (harness.clone(), root.clone());
        async move {
            let live = harness.snapshot(&LIVE_DOC, root.id, &context()).await.unwrap();
            live.and_then(|live| live.get("tools")?.as_array()?.first()?.get("status").cloned())
                == Some(Json::from("running"))
        }
    })
    .await;
}

fn setup() -> ChatSetup {
    chat_setup(RegisterFauxProviderOptions::default())
}

async fn answers(root: &Conversation) -> Vec<pi_durable::EntryId> {
    all_entries(root).await.iter().filter(|entry| entry.kind == "pi.assistant").map(|entry| entry.id).collect()
}

#[tokio::test]
async fn queues_busy_submissions_and_places_writes_before_user_items_at_the_final_boundary_one_follow_up_per_run() {
    let setup = setup();
    let (first, reached, release) = gated("first");
    setup.faux.set_responses([first, answer("second"), answer("third")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let input = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    reached.notified().await;
    let f1 = root.submit(SubmissionDraft::input("f1"), &ctx()).await.unwrap();
    let write = root.submit(SubmissionDraft::write(queued_note("w")), &ctx()).await.unwrap();
    let f2 = root.submit(SubmissionDraft::input("f2").with_when_busy(WhenBusy::FollowUp), &ctx()).await.unwrap();
    for submission in [&f1, &write, &f2] {
        assert_eq!(submission.status(&ctx()).await.unwrap().status, SubmissionStatus::Queued);
    }
    let expected = vec![(f1.id, "followUp".into()), (write.id, "write".into()), (f2.id, "followUp".into())];
    assert_eq!(inbox(&harness, &root).await, expected);
    assert_eq!(transcript(&all_entries(&root).await), ["pi.user:a"]);

    release.resolve();
    f2.wait(&ctx()).await.unwrap();
    assert_eq!(
        transcript(&all_entries(&root).await),
        [
            "pi.user:a",
            "pi.assistant:first",
            "note:w",
            "pi.user:f1",
            "pi.assistant:second",
            "pi.user:f2",
            "pi.assistant:third"
        ]
    );
    let answers = answers(&root).await;
    for (submission, answer) in [(&input, answers[0]), (&f1, answers[1]), (&f2, answers[2])] {
        let status = submission.status(&ctx()).await.unwrap();
        assert_eq!((status.status, status.answer), (SubmissionStatus::Done, Some(answer)));
    }
    assert_eq!(write.status(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    assert!(inbox(&harness, &root).await.is_empty());
    assert_eq!(harness.snapshot(&LIVE_DOC, root.id, &ctx()).await.unwrap(), Some(Json::from(json!({}))));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn places_every_follow_up_in_one_successor_run_with_follow_up_mode_all() {
    let setup = setup();
    let (first, reached, release) = gated("first");
    setup.faux.set_responses([first, answer("both")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    setup.settings.lock().unwrap().follow_up_mode = Some(QueueMode::All);
    root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    reached.notified().await;
    let f1 = root.submit(SubmissionDraft::input("f1"), &ctx()).await.unwrap();
    let f2 = root.submit(SubmissionDraft::input("f2"), &ctx()).await.unwrap();
    release.resolve();
    let settled = f2.wait(&ctx()).await.unwrap();
    let first_status = f1.status(&ctx()).await.unwrap();
    assert_eq!((first_status.status, first_status.answer), (settled.status, settled.answer));
    assert_eq!(
        transcript(&all_entries(&root).await),
        ["pi.user:a", "pi.assistant:first", "pi.user:f1", "pi.user:f2", "pi.assistant:both"]
    );
    assert_eq!(setup.faux.state().call_count, 2);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn adds_steers_to_the_run_at_the_post_tools_boundary_and_holds_follow_ups_for_the_final_boundary() {
    let setup = setup();
    let gate = Deferred::default();
    hold_tool(&setup, gate.clone(), empty_result());
    setup.faux.set_responses([hold_call(), answer("after tools"), answer("follow-up")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let input = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    tool_running(&harness, &root).await;
    let steer = root.submit(SubmissionDraft::input("s").with_when_busy(WhenBusy::Steer), &ctx()).await.unwrap();
    let follow_up = root.submit(SubmissionDraft::input("f"), &ctx()).await.unwrap();
    gate.resolve();
    follow_up.wait(&ctx()).await.unwrap();
    assert_eq!(
        transcript(&all_entries(&root).await),
        [
            "pi.user:a",
            "pi.assistant",
            "pi.tool-result",
            "pi.user:s",
            "pi.assistant:after tools",
            "pi.user:f",
            "pi.assistant:follow-up"
        ]
    );
    let answers = answers(&root).await;
    for (submission, answer) in [(&input, answers[1]), (&steer, answers[1]), (&follow_up, answers[2])] {
        let status = submission.status(&ctx()).await.unwrap();
        assert_eq!((status.status, status.answer), (SubmissionStatus::Done, Some(answer)));
    }
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn ends_the_run_at_a_queued_reset_after_tools_and_runs_earlier_follow_ups_in_the_new_context() {
    let setup = setup();
    let gate = Deferred::default();
    hold_tool(&setup, gate.clone(), empty_result());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let sink = requests.clone();
    let record = FauxResponseStep::factory(move |transcript, _, _, _| {
        let roles: Vec<String> = transcript
            .messages
            .iter()
            .map(|message| {
                let role = match message {
                    Message::System(_) => "system",
                    Message::User(_) => "user",
                    Message::Assistant(_) => "assistant",
                    _ => "other",
                };
                format!("{role}:{}", text_of(Some(message)).unwrap_or_default())
            })
            .collect();
        sink.lock().unwrap().push(roles);
        faux_assistant_message(vec![faux_text("fresh")], FauxMessageOptions::default())
    });
    setup.faux.set_responses([hold_call(), record]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let input = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    tool_running(&harness, &root).await;
    let follow_up = root.submit(SubmissionDraft::input("f"), &ctx()).await.unwrap();
    root.reset(None, &ctx()).await.unwrap();
    gate.resolve();
    follow_up.wait(&ctx()).await.unwrap();
    let status = input.status(&ctx()).await.unwrap();
    assert_eq!((status.status, status.reason.as_deref()), (SubmissionStatus::Unanswered, Some("reset")));
    let entries = all_entries(&root).await;
    assert_eq!(
        transcript(&entries),
        ["pi.user:a", "pi.assistant", "pi.tool-result", "pi.reset", "pi.user:f", "pi.assistant:fresh"]
    );
    let reset = entries.iter().find(|entry| entry.kind == "pi.reset").unwrap();
    assert_eq!(reset.head, Some(reset.id));
    // The follow-up's request starts at the reset: the complete system baseline after the cut leads the follow-up.
    assert_eq!(*requests.lock().unwrap(), [vec!["system:".to_owned(), "user:f".to_owned()]]);
    assert_eq!(setup.faux.state().call_count, 2);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn places_a_queued_reset_after_the_answer_at_the_final_boundary() {
    let setup = setup();
    let (first, reached, release) = gated("first");
    setup.faux.set_responses([first]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let input = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    reached.notified().await;
    root.reset(Some("handoff".into()), &ctx()).await.unwrap();
    release.resolve();
    input.wait(&ctx()).await.unwrap();
    harness.wait_for_idle(&ctx()).await.unwrap();
    assert_eq!(input.status(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    assert_eq!(transcript(&all_entries(&root).await), ["pi.user:a", "pi.assistant:first", "pi.reset:handoff"]);
    let messages = root.context(&ctx(), None).await.unwrap().messages;
    assert_eq!(messages.len(), 1);
    assert_eq!(text_of(messages.first()).as_deref(), Some("handoff"));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn resets_an_idle_conversation_at_once_with_or_without_handoff_text() {
    let setup = setup();
    *setup.now.lock().unwrap() = Some(7.0);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let id = root.id;
    let note = move |tx: pi_durable::session::Tx| async move { tx.append_entry(id, EntryDraft::new("note")).await };
    root.commit(note, &ctx()).await.unwrap();
    root.reset(None, &ctx()).await.unwrap();
    let view = root.context(&ctx(), None).await.unwrap();
    let head = view.head.unwrap();
    assert_eq!((head.kind.as_str(), head.model.is_none()), ("pi.reset", true));
    assert!(view.messages.is_empty());
    root.reset(Some("carry on".into()), &ctx()).await.unwrap();
    let view = root.context(&ctx(), None).await.unwrap();
    let head = view.head.unwrap();
    assert_eq!(head.head, Some(head.id));
    assert_eq!(
        serde_json::to_value(&view.messages).unwrap(),
        json!([{ "role": "user", "content": "carry on", "timestamp": 7 }])
    );
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn withdraws_a_queued_submission_and_removes_its_item() {
    let setup = setup();
    let (first, reached, release) = gated("first");
    setup.faux.set_responses([first]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let input = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    reached.notified().await;
    let kept = root.submit(SubmissionDraft::write(queued_note("kept")), &ctx()).await.unwrap();
    let withdrawn = root.submit(SubmissionDraft::input("f"), &ctx()).await.unwrap();
    assert_eq!(withdrawn.abort(&ctx()).await.unwrap(), AbortSubmissionResult::Aborted);
    let settled = withdrawn.wait(&ctx()).await.unwrap();
    assert_eq!((settled.status, settled.reason.as_deref()), (SubmissionStatus::Unanswered, Some("aborted")));
    assert_eq!(inbox(&harness, &root).await, [(kept.id, "write".to_owned())]);
    release.resolve();
    input.wait(&ctx()).await.unwrap();
    assert_eq!(kept.status(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    assert_eq!(setup.faux.state().call_count, 1);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn leaves_queued_items_after_a_failed_run_until_the_next_submission_places_them_in_order() {
    let setup = setup();
    let error = FauxMessageOptions {
        stop_reason: Some(StopReason::Error),
        error_message: Some("invalid request".into()),
        ..FauxMessageOptions::default()
    };
    let (failing_reached, failing_gate) = (Arc::new(Notify::new()), Deferred::default());
    let (notify, held) = (failing_reached.clone(), failing_gate.clone());
    let failing = FauxResponseStep::async_factory(move |_, _, _, _| {
        let (notify, held, error) = (notify.clone(), held.clone(), error.clone());
        Box::pin(async move {
            notify.notify_one();
            held.wait().await;
            Ok(faux_assistant_message(Vec::new(), error))
        })
    });
    setup.faux.set_responses([failing, answer("for f"), answer("for g")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let input = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    failing_reached.notified().await;
    let f = root.submit(SubmissionDraft::input("f"), &ctx()).await.unwrap();
    failing_gate.resolve();
    let settled = input.wait(&ctx()).await.unwrap();
    assert_eq!((settled.status, settled.reason.as_deref()), (SubmissionStatus::Unanswered, Some("model_error")));
    harness.wait_for_idle(&ctx()).await.unwrap();
    assert_eq!(f.status(&ctx()).await.unwrap().status, SubmissionStatus::Queued);
    assert_eq!(inbox(&harness, &root).await, [(f.id, "followUp".to_owned())]);

    // Idle with a queued item: the new input queues behind it, and a final boundary places the older one first.
    let g = root.submit(SubmissionDraft::input("g").with_when_busy(WhenBusy::Reject), &ctx()).await.unwrap();
    g.wait(&ctx()).await.unwrap();
    assert_eq!(f.status(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    let transcript = transcript(&all_entries(&root).await);
    assert_eq!(
        transcript[transcript.len() - 4..],
        ["pi.user:f", "pi.assistant:for f", "pi.user:g", "pi.assistant:for g"]
    );
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn totals_assistant_usage_per_model_and_tool_usage_per_tool_and_sums_the_session() {
    let setup = setup();
    let spent = Usage {
        input: 1,
        output: 2,
        cache_read: 3,
        cache_write: 4,
        total_tokens: 10,
        cost: Cost { input: 0.1, output: 0.2, cache_read: 0.3, cache_write: 0.4, total: 1.0, ..Cost::default() },
        ..Usage::default()
    };
    let gate = Deferred::default();
    gate.resolve();
    hold_tool(&setup, gate, ToolExecutionResult { usage: Some(spent), ..empty_result() });
    setup.faux.set_responses([hold_call(), answer("done"), answer("other")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let entries = all_entries(&root).await;
    let (mut input, mut output) = (0, 0);
    for entry in entries.iter().filter(|entry| entry.kind == "pi.assistant") {
        if let Some(Message::Assistant(message)) = entry.model.as_ref().and_then(|model| model.first()) {
            input += message.usage.input;
            output += message.usage.output;
        }
    }
    let usage = harness.snapshot(&USAGE_DOC, root.id, &ctx()).await.unwrap().unwrap().to_serde();
    let model = &usage["models"]["faux/faux-1"];
    assert_eq!((model["input"].as_u64(), model["output"].as_u64()), (Some(input), Some(output)));
    let spent_json = Json::from(serde_json::to_value(spent).unwrap());
    assert_eq!(Json::from(usage["tools"]["hold"].clone()), spent_json);

    // A fork starts at zero; the Session total adds every conversation once.
    let fork = root.fork(entries.last().unwrap().id, CreateOptions::default(), &ctx()).await.unwrap();
    let fork_usage = harness.snapshot(&USAGE_DOC, fork.id, &ctx()).await.unwrap().unwrap();
    assert_eq!(fork_usage.to_serde(), json!({ "models": {}, "tools": {} }));
    fork.submit(SubmissionDraft::input("b"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let fork_usage = harness.snapshot(&USAGE_DOC, fork.id, &ctx()).await.unwrap().unwrap().to_serde();
    let fork_output = fork_usage["models"]["faux/faux-1"]["output"].as_u64().unwrap();
    let total = harness.usage(&ctx()).await.unwrap();
    let total_output = total.models.get("faux/faux-1").unwrap().get("output").and_then(Json::as_u64).unwrap();
    assert_eq!(total_output, output + fork_output);
    assert_eq!(total.tools.get("hold").cloned(), Some(spent_json));
    harness.close(&ctx()).await.unwrap();
}
