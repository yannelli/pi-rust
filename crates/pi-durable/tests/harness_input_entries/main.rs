//! Port of upstream `test/harness-input-entries.test.ts`: inputs placed as application entries (#10407). The data-only
//! write cases are in `writes.rs`. No counterpart for an untyped caller's extra fields: `InputEntry` has only a kind
//! and data.

#[path = "../support/mod.rs"]
mod support;
mod writes;

use std::sync::{Arc, Mutex};

use pi_ai::{AssistantMessage, Message, StopReason, UserContent, UserMessage};
use pi_chord::Json;
use pi_durable::harness::inbox::INBOX_DOC;
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::harness::*;
use pi_durable::storage::MemoryStorage;
use pi_durable::testing::faux::{
    FauxMessageOptions, FauxResponseStep, faux_assistant_message, faux_text, faux_tool_call_with_id,
};
use pi_durable::{DurableError, EntryRecord, SubmissionStatus};
use serde_json::{Value, json};
use support::Deferred;
use support::chat::answer;
use support::harness_support::*;
use tokio::sync::Notify;

/// A faux response held until released or cancelled; `reached` fires when the request is sent.
pub fn gated(text: &str) -> (FauxResponseStep, Arc<Notify>, Deferred) {
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

/// Register a `hold` tool whose calls wait for `gate`.
pub fn hold_tool(setup: &ChatSetup, gate: Deferred) {
    let tool = define_tool("hold", "Waits for the test", json!({ "type": "object" }), move |_, _, _| {
        let gate = gate.clone();
        async move {
            gate.wait().await;
            Ok(ToolExecutionResult::text("held"))
        }
    });
    setup.registry.install(Extension::new("tool:hold").tool(tool)).unwrap();
}

pub fn hold_message() -> AssistantMessage {
    let call = faux_tool_call_with_id("hold", json!({}), "c1");
    faux_assistant_message(vec![call], FauxMessageOptions::stop_reason(StopReason::ToolUse))
}

pub fn reply(text: &str) -> AssistantMessage {
    faux_assistant_message(vec![faux_text(text)], FauxMessageOptions::default())
}

/// A faux response that records the role and text of each request message, then replies with `message`.
pub fn request_log(requests: &Arc<Mutex<Vec<Vec<String>>>>, message: AssistantMessage) -> FauxResponseStep {
    let sink = requests.clone();
    FauxResponseStep::factory(move |request, _, _, _| {
        let rows = request.messages.iter().map(|message| {
            let role = match message {
                Message::System(_) => "system",
                Message::User(_) => "user",
                Message::Assistant(_) => "assistant",
                Message::ToolResult(_) => "toolResult",
                Message::Custom(_) => "custom",
            };
            format!("{role}:{}", text_of(Some(message)).unwrap_or_default())
        });
        sink.lock().unwrap().push(rows.collect());
        message.clone()
    })
}

/// Kind, application data, and model text of each entry, skipping system entries.
pub fn transcript(entries: &[EntryRecord]) -> Value {
    let rows = entries.iter().filter(|entry| entry.kind != "pi.system").map(|entry| {
        let message = entry.model.as_ref().and_then(|model| model.first());
        let text = match message {
            Some(Message::ToolResult(_)) => None,
            other => text_of(other).filter(|text| !text.is_empty()),
        };
        let data = entry.data.as_ref().filter(|_| !entry.kind.starts_with("pi."));
        let mut row = vec![json!(entry.kind)];
        row.extend(data.map(Json::to_serde));
        row.extend(text.map(Value::from));
        Value::from(row)
    });
    Value::from(rows.collect::<Vec<_>>())
}

pub async fn inbox_items(harness: &Harness, root: &Conversation) -> Vec<Value> {
    let state = harness.snapshot(&INBOX_DOC, root.id, &ctx()).await.unwrap().unwrap().to_serde();
    state["items"].as_array().unwrap().clone()
}

pub async fn tool_running(harness: &Harness, root: &Conversation) {
    wait_for(|| {
        let (harness, root) = (harness.clone(), root.clone());
        async move {
            let live = harness.snapshot(&LIVE_DOC, root.id, &ctx()).await.unwrap();
            live.and_then(|live| live.get("tools")?.as_array()?.first()?.get("status").cloned())
                == Some(Json::from("running"))
        }
    })
    .await;
}

pub fn mention(text: &str) -> InputEntry {
    InputEntry::new("chat.mention").with_data(Json::from(json!({ "author": "ann", "text": text })))
}

fn user_text(text: &str, timestamp: i64) -> Message {
    Message::User(UserMessage { content: UserContent::Text(text.into()), timestamp })
}

#[tokio::test]
async fn places_an_idle_input_as_its_entry_the_given_kind_and_data_with_the_content_as_its_user_message() {
    let setup = chat_setup(Default::default());
    *setup.now.lock().unwrap() = Some(42.0);
    let requests = Arc::new(Mutex::new(Vec::new()));
    setup.faux.set_responses([request_log(&requests, reply("hi ann"))]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let draft = SubmissionDraft::input("ann: hi").with_entry(mention("hi"));
    let settled = root.submit(draft, &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let entries = all_entries(&root).await;
    let row = json!(["chat.mention", { "author": "ann", "text": "hi" }, "ann: hi"]);
    assert_eq!(transcript(&entries), json!([row, ["pi.assistant", "hi ann"]]));
    let placed = entries.iter().find(|entry| entry.kind == "chat.mention").unwrap();
    assert_eq!(placed.model, Some(vec![user_text("ann: hi", 42)]));
    assert_eq!(settled.status, SubmissionStatus::Done);
    assert_eq!((settled.entry, settled.answer), (Some(placed.id), Some(entries.last().unwrap().id)));
    assert_eq!(*requests.lock().unwrap(), [["user:ann: hi"]]);
    // The context of a fork at the input is the same as for a pi.user entry.
    let view = root.context(&ctx(), Some(placed.id)).await.unwrap();
    let messages: Vec<_> = view.messages.into_iter().filter(|message| !matches!(message, Message::System(_))).collect();
    assert_eq!(Some(messages), placed.model);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn rejects_an_input_entry_whose_kind_is_empty_or_starts_with_pi_writing_nothing() {
    let setup = chat_setup(Default::default());
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let before = all_entries(&root).await.len();
    for kind in ["", "pi.user", "pi.compaction"] {
        let draft = SubmissionDraft::input("x").with_entry(InputEntry::new(kind));
        let error = root.submit(draft, &ctx()).await.err().unwrap();
        assert!(error.to_string().contains("An input entry's kind must be"), "{error}");
    }
    assert_eq!(all_entries(&root).await.len(), before);
    assert_eq!(harness.snapshot(&LIVE_DOC, root.id, &ctx()).await.unwrap(), Some(Json::from(json!({}))));
    // Writes keep their kinds: reset() writes pi.reset.
    root.reset(None, &ctx()).await.unwrap();
    assert_eq!(all_entries(&root).await.last().unwrap().kind, "pi.reset");
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn queues_busy_inputs_with_their_entries_in_the_inbox_and_places_them_as_those_entries_at_boundaries() {
    let setup = chat_setup(Default::default());
    let gate = Deferred::default();
    hold_tool(&setup, gate.clone());
    setup.faux.set_responses([hold_message().into(), answer("after tools"), answer("follow-up")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let input = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    tool_running(&harness, &root).await;
    let steering = |text: &str| SubmissionDraft::input(text).with_entry(mention(text)).with_when_busy(WhenBusy::Steer);
    let steer = root.submit(steering("s").with_request_id("r-s"), &ctx()).await.unwrap();
    let plain = SubmissionDraft::input("f").with_entry(InputEntry::new("p2.input")).with_when_busy(WhenBusy::FollowUp);
    let follow_up = root.submit(plain, &ctx()).await.unwrap();
    let mentioned = json!({ "kind": "chat.mention", "data": { "author": "ann", "text": "s" } });
    let expected = [
        json!({ "id": steer.id, "mode": "steer", "content": "s", "entry": mentioned }),
        json!({ "id": follow_up.id, "mode": "followUp", "content": "f", "entry": { "kind": "p2.input" } }),
    ];
    assert_eq!(inbox_items(&harness, &root).await, expected);
    // The request ID deduplicates a queued input with an entry as any other.
    let again = root.submit(steering("other").with_request_id("r-s"), &ctx()).await.unwrap();
    assert_eq!(again.id, steer.id);
    let rejected = SubmissionDraft::input("x").with_entry(mention("x")).with_when_busy(WhenBusy::Reject);
    let error = root.submit(rejected, &ctx()).await.err().unwrap();
    assert!(matches!(error, DurableError::ConversationBusy(_)));
    assert_eq!(inbox_items(&harness, &root).await.len(), 2);

    gate.resolve();
    follow_up.wait(&ctx()).await.unwrap();
    let entries = all_entries(&root).await;
    let mentioned = json!(["chat.mention", { "author": "ann", "text": "s" }, "s"]);
    let expected = json!([
        ["pi.user", "a"],
        ["pi.assistant"],
        ["pi.tool-result"],
        mentioned,
        ["pi.assistant", "after tools"],
        ["p2.input", "f"],
        ["pi.assistant", "follow-up"]
    ]);
    assert_eq!(transcript(&entries), expected);
    let answers: Vec<_> = entries.iter().filter(|entry| entry.kind == "pi.assistant").map(|entry| entry.id).collect();
    let placed = |kind: &str| entries.iter().find(|entry| entry.kind == kind).map(|entry| entry.id);
    let shown = |record: pi_durable::SubmissionRecord| (record.status, record.entry, record.answer);
    let done = SubmissionStatus::Done;
    assert_eq!(input.status(&ctx()).await.unwrap().answer, Some(answers[1]));
    assert_eq!(shown(steer.status(&ctx()).await.unwrap()), (done, placed("chat.mention"), Some(answers[1])));
    assert_eq!(shown(follow_up.status(&ctx()).await.unwrap()), (done, placed("p2.input"), Some(answers[2])));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn withdraws_queued_inputs_with_entries_one_by_one_and_by_conversation_abort() {
    let setup = chat_setup(Default::default());
    let (first, reached, _release) = gated("first");
    setup.faux.set_responses([first]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let input = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    reached.notified().await;
    let withdrawn = root.submit(SubmissionDraft::input("w").with_entry(mention("w")), &ctx()).await.unwrap();
    let aborted = root.submit(SubmissionDraft::input("x").with_entry(mention("x")), &ctx()).await.unwrap();
    assert_eq!(withdrawn.abort(&ctx()).await.unwrap(), AbortSubmissionResult::Aborted);
    let ids: Vec<_> = inbox_items(&harness, &root).await.iter().map(|item| item["id"].clone()).collect();
    assert_eq!(ids, [json!(aborted.id)]);
    root.abort(&ctx(), false).await.unwrap();
    for submission in [&withdrawn, &aborted, &input] {
        let settled = submission.wait(&ctx()).await.unwrap();
        assert_eq!((settled.status, settled.reason.as_deref()), (SubmissionStatus::Unanswered, Some("aborted")));
    }
    assert!(inbox_items(&harness, &root).await.is_empty());
    assert!(all_entries(&root).await.iter().all(|entry| entry.kind != "chat.mention"));
    harness.close(&ctx()).await.unwrap();
}
