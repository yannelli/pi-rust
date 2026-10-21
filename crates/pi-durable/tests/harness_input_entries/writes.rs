//! Data-only writes (only `kind` and `data`) are appended at once, busy or not (#10407).

use pi_chord::Json;
use pi_durable::storage::open_sqlite_storage;
use pi_durable::{ContextEdit, EntryDraft};
use serde_json::{Value, json};

use super::*;

fn chat_message(data: Value) -> EntryDraft {
    EntryDraft { data: Some(Json::from(data)), ..EntryDraft::new("chat.message") }
}

async fn dormant(harness: &Harness, root: &Conversation) -> Option<Json> {
    harness.snapshot(&LIVE_DOC, root.id, &ctx()).await.unwrap()
}

#[tokio::test]
async fn appends_a_data_only_write_mid_run_in_time_order_leaving_the_models_messages_unchanged() {
    let setup = chat_setup(Default::default());
    let gate = Deferred::default();
    hold_tool(&setup, gate.clone());
    let requests = Arc::new(Mutex::new(Vec::new()));
    setup.faux.set_responses([request_log(&requests, hold_message()), request_log(&requests, reply("done"))]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let events = Arc::new(Mutex::new(Vec::<Value>::new()));
    let stream = watch_events(&harness, root.id, &ctx()).await.unwrap();
    let sink = events.clone();
    stream
        .start(move |batch, _| {
            sink.lock().unwrap().extend(batch.iter().map(Value::from));
            async { Ok(()) }
        })
        .unwrap();
    let input = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    tool_running(&harness, &root).await;
    let data = json!({ "author": "bob", "text": "lunch?" });
    let draft = || SubmissionDraft::write(chat_message(data.clone())).with_request_id("bob:1");
    let write = root.submit(draft(), &ctx()).await.unwrap();
    // Settled and appended in its own commit, between the call and its still-running tool's result.
    let settled = write.status(&ctx()).await.unwrap();
    assert_eq!(settled.status, SubmissionStatus::Done);
    assert!(inbox_items(&harness, &root).await.is_empty());
    let mid = json!([["pi.user", "a"], ["pi.assistant"], ["chat.message", data]]);
    assert_eq!(transcript(&all_entries(&root).await), mid);
    let appended = |events: &Mutex<Vec<Value>>| {
        let events = events.lock().unwrap();
        events.iter().find(|event| event["type"] == "entry_appended").cloned()
    };
    wait_for(|| {
        let events = events.clone();
        async move { events.lock().unwrap().iter().any(|event| event["type"] == "entry_appended") }
    })
    .await;
    let appended = appended(&events).unwrap();
    assert_eq!(appended["entry"]["kind"], "chat.message");
    assert_eq!(appended["entry"]["id"], json!(settled.entry.unwrap()));
    assert!(events.lock().unwrap().iter().all(|event| event["type"] != "run_end"));
    // A repeated request ID returns the same write without a second entry.
    assert_eq!(root.submit(draft(), &ctx()).await.unwrap().id, write.id);

    gate.resolve();
    input.wait(&ctx()).await.unwrap();
    let data = json!({ "author": "bob", "text": "lunch?" });
    let expected = json!([
        ["pi.user", "a"],
        ["pi.assistant"],
        ["chat.message", data],
        ["pi.tool-result"],
        ["pi.assistant", "done"]
    ]);
    assert_eq!(transcript(&all_entries(&root).await), expected);
    let requests = requests.lock().unwrap().clone();
    assert_eq!(requests, [vec!["system:", "user:a"], vec!["system:", "user:a", "assistant:", "toolResult:held"]]);
    stream.stop().await;
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn queues_a_data_only_write_behind_a_queued_reset_so_it_lands_after_the_reset_and_stays_visible() {
    let setup = chat_setup(Default::default());
    let (first, reached, release) = gated("first");
    setup.faux.set_responses([first]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let input = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    reached.notified().await;
    root.reset(None, &ctx()).await.unwrap();
    let chat = root.submit(SubmissionDraft::write(chat_message(json!("hi"))), &ctx()).await.unwrap();
    assert_eq!(chat.status(&ctx()).await.unwrap().status, SubmissionStatus::Queued);
    release.resolve();
    input.wait(&ctx()).await.unwrap();
    let settled = chat.wait(&ctx()).await.unwrap();
    let entries = all_entries(&root).await;
    let reset = entries.iter().find(|entry| entry.kind == "pi.reset").unwrap();
    assert!(settled.entry.unwrap() > reset.id);
    let view = root.context(&ctx(), None).await.unwrap();
    assert_eq!(kinds(&view.entries), ["pi.reset", "chat.message"]);
    // Once the reset is placed, display-only writes go in at once again.
    let later = root.submit(SubmissionDraft::write(chat_message(json!("again"))), &ctx()).await.unwrap();
    assert_eq!(later.status(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn leaves_the_request_unchanged_by_a_data_only_write_that_lands_while_the_generation_prepares() {
    let setup = chat_setup(Default::default());
    let (rendering, render) = (Deferred::default(), Deferred::default());
    let renders = Arc::new(Mutex::new(0));
    let (count, started, gate) = (renders.clone(), rendering.clone(), render.clone());
    let slow = section(
        "slow",
        move |_, _| {
            let (count, started, gate) = (count.clone(), started.clone(), gate.clone());
            async move {
                let nth = {
                    let mut count = count.lock().unwrap();
                    *count += 1;
                    *count
                };
                if nth == 2 {
                    started.resolve();
                    gate.wait().await;
                }
                Ok(Some("SLOW".to_owned()))
            }
        },
        None,
    );
    setup.registry.install(Extension::new("section:slow").section(slow)).unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    setup.faux.set_responses([request_log(&requests, reply("one")), request_log(&requests, reply("two"))]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let second = root.submit(SubmissionDraft::input("b"), &ctx()).await.unwrap();
    rendering.wait().await;
    root.submit(SubmissionDraft::write(chat_message(json!("hi"))), &ctx()).await.unwrap();
    render.resolve();
    second.wait(&ctx()).await.unwrap();
    let rows = ["system:", "user:a", "assistant:one", "user:b"];
    let second_request = requests.lock().unwrap()[1].clone();
    assert_eq!(second_request.len(), rows.len());
    assert!(second_request.iter().zip(rows).all(|(sent, row)| sent.starts_with(row)), "{second_request:?}");
    let kinds = kinds(&all_entries(&root).await);
    // The unchanged prompt adds no second pi.system entry.
    assert_eq!(kinds, ["pi.user", "pi.system", "pi.assistant", "pi.user", "chat.message", "pi.assistant"]);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn appends_a_data_only_write_while_idle_after_a_failed_run_without_placing_the_queued_items() {
    let setup = chat_setup(Default::default());
    let (reached, gate) = (Arc::new(Notify::new()), Deferred::default());
    let (notify, held) = (reached.clone(), gate.clone());
    let failing = FauxResponseStep::async_factory(move |_, _, _, _| {
        let (notify, held) = (notify.clone(), held.clone());
        Box::pin(async move {
            notify.notify_one();
            held.wait().await;
            let error = FauxMessageOptions {
                stop_reason: Some(StopReason::Error),
                error_message: Some("invalid request".into()),
                ..FauxMessageOptions::default()
            };
            Ok(faux_assistant_message(Vec::new(), error))
        })
    });
    setup.faux.set_responses([failing, answer("for f")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let input = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    reached.notified().await;
    let f = root.submit(SubmissionDraft::input("f").with_entry(mention("f")), &ctx()).await.unwrap();
    gate.resolve();
    let settled = input.wait(&ctx()).await.unwrap();
    assert_eq!((settled.status, settled.reason.as_deref()), (SubmissionStatus::Unanswered, Some("model_error")));
    harness.wait_for_idle(&ctx()).await.unwrap();
    let note = EntryDraft { data: Some(Json::from("n")), ..EntryDraft::new("note") };
    let note = root.submit(SubmissionDraft::write(note), &ctx()).await.unwrap();
    assert_eq!(note.status(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    harness.wait_for_idle(&ctx()).await.unwrap();
    assert_eq!(f.status(&ctx()).await.unwrap().status, SubmissionStatus::Queued);
    let ids: Vec<_> = inbox_items(&harness, &root).await.iter().map(|item| item["id"].clone()).collect();
    assert_eq!(ids, [json!(f.id)]);
    assert_eq!(dormant(&harness, &root).await, Some(Json::from(json!({}))));
    assert_eq!(setup.faux.state().call_count, 1);
    // The next input places the waiting mention first.
    let g = root.submit(SubmissionDraft::input("g"), &ctx()).await.unwrap();
    g.wait(&ctx()).await.unwrap();
    let rows = transcript(&all_entries(&root).await);
    let rows: Vec<_> = rows.as_array().unwrap().iter().filter(|row| row[0] != "pi.assistant").cloned().collect();
    let mentioned = json!(["chat.mention", { "author": "ann", "text": "f" }, "f"]);
    assert_eq!(rows, [json!(["pi.user", "a"]), json!(["note", "n"]), mentioned, json!(["pi.user", "g"])]);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn keeps_a_queued_inputs_entry_across_reopen_and_places_it_as_that_entry() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite").to_string_lossy().into_owned();
    let setup = chat_setup(Default::default());
    let (first, reached, _release) = gated("first");
    setup.faux.set_responses([first, answer("first again"), answer("for f")]);
    let storage = || async { Arc::new(open_sqlite_storage(&path).await.unwrap()) };
    let (harness, root) = open_chat(storage().await, &setup).await;
    root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    reached.notified().await;
    let f = root.submit(SubmissionDraft::input("f").with_entry(mention("f")), &ctx()).await.unwrap().id;
    harness.close(&ctx()).await.unwrap();

    let (harness, root) = open_chat(storage().await, &setup).await;
    let entry = json!({ "kind": "chat.mention", "data": { "author": "ann", "text": "f" } });
    let queued = inbox_items(&harness, &root).await.into_iter().find(|item| item["id"] == json!(f)).unwrap();
    assert_eq!(queued["entry"], entry);
    let settled = harness.submission(f, &ctx()).await.unwrap().unwrap().wait(&ctx()).await.unwrap();
    let placed = all_entries(&root).await.into_iter().find(|entry| entry.kind == "chat.mention").unwrap();
    assert_eq!((settled.status, settled.entry), (SubmissionStatus::Done, Some(placed.id)));
    assert_eq!(placed.data.as_ref().map(Json::to_serde), Some(entry["data"].clone()));
    assert_eq!(text_of(placed.model.as_ref().and_then(|model| model.first())).as_deref(), Some("f"));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn appends_a_data_only_write_at_once_behind_a_non_empty_inbox_and_still_queues_writes_with_model_or_edits() {
    let setup = chat_setup(Default::default());
    let (first, reached, release) = gated("first");
    setup.faux.set_responses([first, answer("second")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let input = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    reached.notified().await;
    let follow_up = root.submit(SubmissionDraft::input("f"), &ctx()).await.unwrap();
    let model = root.submit(SubmissionDraft::write(queued_note("m")), &ctx()).await.unwrap();
    let target = all_entries(&root).await.into_iter().find(|entry| entry.kind == "pi.user").unwrap().id;
    let redact = EntryDraft { edits: Some(vec![ContextEdit::omit(target)]), ..EntryDraft::new("redact") };
    let edit = root.submit(SubmissionDraft::write(redact), &ctx()).await.unwrap();
    let note = EntryDraft { data: Some(Json::from("n")), ..EntryDraft::new("note") };
    let note = root.submit(SubmissionDraft::write(note), &ctx()).await.unwrap();
    assert_eq!(note.status(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    let modes: Vec<_> =
        inbox_items(&harness, &root).await.iter().map(|item| (item["id"].clone(), item["mode"].clone())).collect();
    let expected = [(follow_up.id, "followUp"), (model.id, "write"), (edit.id, "write")];
    assert_eq!(modes, expected.map(|(id, mode)| (json!(id), json!(mode))));
    release.resolve();
    follow_up.wait(&ctx()).await.unwrap();
    let expected = json!([
        ["pi.user", "a"],
        ["note", "n"],
        ["pi.assistant", "first"],
        ["note", "m"],
        ["redact"],
        ["pi.user", "f"],
        ["pi.assistant", "second"]
    ]);
    assert_eq!(transcript(&all_entries(&root).await), expected);
    assert_eq!(input.status(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    harness.close(&ctx()).await.unwrap();
}
