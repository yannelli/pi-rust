//! The remaining upstream inbox storage cases (reopen, positional operations, bases) and usage cases.

use pi_ai::AssistantMessage;
use pi_chord::delta::Op;
use pi_durable::documents::CheckpointInfo;
use pi_durable::harness::tool::{ToolHookCall, ToolHooks, tool_task};
use pi_durable::harness::usage::{UsageBucket, record_usage};
use pi_durable::session::Tx;
use pi_durable::storage::open_sqlite_storage;
use pi_durable::testing::faux::FauxTokenSize;
use pi_durable::types::DocumentContent;
use pi_durable::{DocumentCommitChange, DocumentId, StorageWrite, TaskId};
use support::{ControlledStorage, commit_observer, document_changes};

use super::*;

type Ops = Arc<Mutex<Vec<Vec<serde_json::Value>>>>;

/// Record the operations of every `pi.inbox` change that has some.
fn record_inbox_ops(harness: &Harness) -> Ops {
    let ops = Ops::default();
    let sink = ops.clone();
    let unsubscribe = harness.observe_commits(commit_observer(move |publication, _| {
        for change in document_changes(publication) {
            if let DocumentCommitChange::Change { record, ops, .. } = change
                && record.kind == "pi.inbox"
                && !ops.is_empty()
            {
                sink.lock().unwrap().push(ops.iter().map(|op| serde_json::to_value(op).unwrap()).collect());
            }
        }
        Ok(())
    }));
    std::mem::forget(unsubscribe);
    ops
}

#[tokio::test]
async fn keeps_queued_submissions_across_reopen_and_settles_them_afterwards() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite").to_string_lossy().into_owned();
    let setup = setup();
    let (first, reached, _release) = gated("first");
    setup.faux.set_responses([first, answer("first again"), answer("f")]);
    let (harness, root) = open_chat(Arc::new(open_sqlite_storage(&path).await.unwrap()), &setup).await;
    root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    reached.notified().await;
    let f = root.submit(SubmissionDraft::input("f"), &ctx()).await.unwrap().id;
    harness.close(&ctx()).await.unwrap();

    let (harness, root) = open_chat(Arc::new(open_sqlite_storage(&path).await.unwrap()), &setup).await;
    let settled = harness.submission(f, &ctx()).await.unwrap().unwrap().wait(&ctx()).await.unwrap();
    assert_eq!(settled.status, SubmissionStatus::Done);
    let transcript = transcript(&all_entries(&root).await);
    assert_eq!(transcript[transcript.len() - 2..], ["pi.user:f", "pi.assistant:f"]);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn commits_inbox_changes_as_positional_chord_operations_and_a_base_when_empty() {
    let setup = setup();
    let (first, reached, release) = gated("first");
    setup.faux.set_responses([first, answer("second")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let ops = record_inbox_ops(&harness);
    let input = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    reached.notified().await;
    let note = || SubmissionDraft::write(queued_note("w"));
    let w1 = root.submit(note(), &ctx()).await.unwrap();
    let f1 = root.submit(SubmissionDraft::input("f1"), &ctx()).await.unwrap();
    let w2 = root.submit(note(), &ctx()).await.unwrap();
    let f2 = root.submit(SubmissionDraft::input("f2"), &ctx()).await.unwrap();
    let w3 = root.submit(note(), &ctx()).await.unwrap();
    let message = json!([{ "role": "user", "content": "w", "timestamp": 0 }]);
    let write = |id: SubmissionId| json!({ "id": id, "mode": "write", "entry": { "kind": "note", "model": message } });
    let follow = |id: SubmissionId, text: &str| json!({ "id": id, "mode": "followUp", "content": text });
    let pushes = [write(w1.id), follow(f1.id, "f1"), write(w2.id), follow(f2.id, "f2"), write(w3.id)];
    let expected: Vec<Vec<serde_json::Value>> =
        pushes.into_iter().enumerate().map(|(index, item)| vec![json!(["p", ["items"], index, 0, [item]])]).collect();
    assert_eq!(*ops.lock().unwrap(), expected);
    release.resolve();
    input.wait(&ctx()).await.unwrap();
    // Every write and the first follow-up leave; only f2 at index 3 remains. No old value is carried.
    let removal = ops.lock().unwrap()[5].clone();
    assert!(removal.iter().all(|op| op[0] == "p" && op[4].as_array().is_some_and(Vec::is_empty)));
    // Upstream records one splice per item, back to front; a Rust draft diffs the edited list, so its removals
    // are coalesced runs. Both leave only f2.
    assert_eq!(removal, [json!(["p", ["items"], 0, 3, []]), json!(["p", ["items"], 1, 1, []])]);
    assert!(!serde_json::to_string(&removal).unwrap().contains("f2"));
    f2.wait(&ctx()).await.unwrap();
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn withdraws_a_middle_item_with_one_positional_removal() {
    let setup = setup();
    let (first, reached, _release) = gated("first");
    setup.faux.set_responses([first]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    reached.notified().await;
    let mut items = Vec::new();
    for text in ["x", "y", "z"] {
        items.push(root.submit(SubmissionDraft::input(text), &ctx()).await.unwrap());
    }
    let ops = record_inbox_ops(&harness);
    items[1].abort(&ctx()).await.unwrap();
    assert_eq!(*ops.lock().unwrap(), [vec![json!(["p", ["items"], 1, 1, []])]]);
    let expected = [(items[0].id, "followUp".to_owned()), (items[2].id, "followUp".to_owned())];
    assert_eq!(inbox(&harness, &root).await, expected);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn stores_the_inbox_as_a_base_exactly_when_it_becomes_empty() {
    let setup = setup();
    let (first, reached, release) = gated("first");
    setup.faux.set_responses([first, answer("second")]);
    let storage = Arc::new(ControlledStorage::default());
    let (harness, root) = open_chat(storage.clone(), &setup).await;
    let inbox_id: Arc<Mutex<Option<DocumentId>>> = Arc::default();
    let sink = inbox_id.clone();
    let unsubscribe = harness.observe_commits(commit_observer(move |publication, _| {
        for change in document_changes(publication) {
            if let DocumentCommitChange::Change { record, .. } | DocumentCommitChange::Retire { record, .. } = change
                && record.kind == "pi.inbox"
            {
                *sink.lock().unwrap() = Some(record.id);
            }
        }
        Ok(())
    }));
    std::mem::forget(unsubscribe);
    let input = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    reached.notified().await;
    let start = storage.commit_count();
    root.submit(SubmissionDraft::input("f1"), &ctx()).await.unwrap();
    let f2 = root.submit(SubmissionDraft::input("f2"), &ctx()).await.unwrap();
    release.resolve();
    input.wait(&ctx()).await.unwrap();
    f2.wait(&ctx()).await.unwrap();
    let id = inbox_id.lock().unwrap().unwrap();
    // Upstream learns the inbox ID from the f1 push; later writes: the f2 push, removing f1, and emptying the inbox.
    let written: Vec<(&str, bool)> = storage.commits()[start + 1..]
        .iter()
        .flatten()
        .filter_map(|write| match write {
            StorageWrite::DocumentChange { id: changed, content } if *changed == id => Some(match content {
                DocumentContent::Base { value, .. } => ("base", value.get("items").unwrap().to_serde() == json!([])),
                DocumentContent::Delta { .. } => ("delta", false),
            }),
            _ => None,
        })
        .collect();
    assert_eq!(written, [("delta", false), ("delta", false), ("base", true)]);
    harness.close(&ctx()).await.unwrap();
}

#[test]
fn keeps_a_complete_inbox_base_exactly_while_it_is_empty_and_a_usage_base_on_every_change() {
    let info = || CheckpointInfo { deltas_since_base: 1000 };
    let inbox = INBOX_DOC.definition();
    let item = json!({ "id": 1, "mode": "followUp", "content": "x" });
    assert!(inbox.checkpoint_when(&Json::from(json!({ "items": [] })), &[], info()).unwrap());
    assert!(!inbox.checkpoint_when(&Json::from(json!({ "items": [item] })), &[], info()).unwrap());
    let usage = USAGE_DOC.definition();
    assert!(usage.checkpoint_when(&Json::from(json!({ "models": {}, "tools": {} })), &[], info()).unwrap());
}

fn assistants(entries: &[EntryRecord]) -> Vec<AssistantMessage> {
    let messages = entries.iter().filter(|entry| entry.kind == "pi.assistant");
    messages
        .filter_map(|entry| match entry.model.as_ref()?.first()? {
            Message::Assistant(message) => Some(message.clone()),
            _ => None,
        })
        .collect()
}

fn paced() -> ChatSetup {
    let token_size = Some(FauxTokenSize { min: Some(1), max: Some(1) });
    chat_setup(RegisterFauxProviderOptions { tokens_per_second: Some(200.0), token_size, ..Default::default() })
}

async fn live_message(harness: &Harness, root: &Conversation) {
    wait_for(|| {
        let (harness, root) = (harness.clone(), root.clone());
        async move {
            let live = harness.snapshot(&LIVE_DOC, root.id, &ctx()).await.unwrap();
            live.and_then(|live| live.get("generation")?.get("message").cloned()).is_some()
        }
    })
    .await;
}

fn usage(input: u64, output: u64, total_tokens: u64) -> Usage {
    Usage { input, output, total_tokens, cost: Cost::default(), ..Usage::default() }
}

#[tokio::test]
async fn counts_failed_attempts_converted_partials_and_tool_usage_replaced_by_after_tool() {
    let setup = paced();
    let spent = usage(5, 0, 5);
    let gate = Deferred::default();
    gate.resolve();
    hold_tool(&setup, gate, empty_result());
    let hooks = ToolHooks {
        before_tool: None,
        after_tool: Some(Arc::new(move |(_, result): (ToolHookCall, ToolExecutionResult), _, _| {
            Box::pin(async move { Ok(Some(ToolExecutionResult { usage: Some(spent), ..result })) })
        })),
    };
    setup.registry.install(Extension::new("hooks").hook(hook(&tool_task(), hooks))).unwrap();
    let options = FauxMessageOptions {
        stop_reason: Some(StopReason::Error),
        error_message: Some("503 Service Unavailable".into()),
        ..FauxMessageOptions::default()
    };
    let retryable: FauxResponseStep = faux_assistant_message(Vec::new(), options).into();
    setup.faux.set_responses([retryable, hold_call(), answer("done"), answer(&"x".repeat(400))]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let retry =
        RetryOverrides { enabled: Some(true), max_retries: Some(1), base_delay_ms: Some(1), ..Default::default() };
    setup.settings.lock().unwrap().retry = Some(retry);
    root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let ledger = harness.snapshot(&USAGE_DOC, root.id, &ctx()).await.unwrap().unwrap().to_serde();
    assert_eq!(
        Json::from(ledger["tools"].clone()),
        Json::from(json!({ "hold": serde_json::to_value(spent).unwrap() }))
    );

    // A partial committed while streaming becomes an aborted entry on abort; its usage counts too.
    let input = root.submit(SubmissionDraft::input("b"), &ctx()).await.unwrap();
    live_message(&harness, &root).await;
    let live = harness.snapshot(&LIVE_DOC, root.id, &ctx()).await.unwrap().unwrap().to_serde();
    let task_id: TaskId = serde_json::from_value(live["run"]["taskId"].clone()).unwrap();
    harness.abort_task(task_id, &ctx()).await.unwrap();
    input.wait(&ctx()).await.unwrap();
    let assistants = assistants(&all_entries(&root).await);
    let stops: Vec<StopReason> = assistants.iter().map(|message| message.stop_reason).collect();
    assert_eq!(stops, [StopReason::Error, StopReason::ToolUse, StopReason::Stop, StopReason::Aborted]);
    let output: u64 = assistants.iter().map(|message| message.usage.output).sum();
    let ledger = harness.snapshot(&USAGE_DOC, root.id, &ctx()).await.unwrap().unwrap().to_serde();
    assert_eq!(ledger["models"]["faux/faux-1"]["output"].as_u64(), Some(output));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn keeps_tools_named_like_object_prototype_keys_in_the_ledger_and_the_session_total() {
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup()).await;
    let names = ["constructor", "__proto__", "toString"];
    for name in names {
        for _ in 0..2 {
            let id = root.id;
            let record =
                move |tx: Tx| async move { record_usage(&tx, id, UsageBucket::Tools, name, &usage(1, 1, 2)).await };
            root.commit(record, &ctx()).await.unwrap();
        }
    }
    let total = serde_json::to_value(harness.usage(&ctx()).await.unwrap()).unwrap();
    let tools = total["tools"].as_object().unwrap();
    let mut keys: Vec<&str> = tools.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["__proto__", "constructor", "toString"]);
    for name in names {
        assert_eq!(tools[name]["output"], 2);
    }
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn keeps_usage_totals_exact_across_reopen_counting_a_partial_converted_after_reopen_once() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite").to_string_lossy().into_owned();
    let setup = paced();
    setup.faux.set_responses([answer("first"), answer(&"x".repeat(400)), answer("again")]);
    let (harness, root) = open_chat(Arc::new(open_sqlite_storage(&path).await.unwrap()), &setup).await;
    root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    root.submit(SubmissionDraft::input("b"), &ctx()).await.unwrap();
    live_message(&harness, &root).await;
    // Closing mid-stream keeps the committed partial; the reopened request converts it into an aborted entry.
    harness.close(&ctx()).await.unwrap();
    let (harness, root) = open_chat(Arc::new(open_sqlite_storage(&path).await.unwrap()), &setup).await;
    harness.resume().unwrap();
    harness.wait_for_idle(&ctx()).await.unwrap();
    let assistants = assistants(&all_entries(&root).await);
    let stops: Vec<StopReason> = assistants.iter().map(|message| message.stop_reason).collect();
    assert_eq!(stops, [StopReason::Stop, StopReason::Aborted, StopReason::Stop]);
    let total = serde_json::to_value(harness.usage(&ctx()).await.unwrap()).unwrap();
    let model = &total["models"]["faux/faux-1"];
    let sum = |field: fn(&Usage) -> u64| assistants.iter().map(|message| field(&message.usage)).sum::<u64>();
    let expected = [sum(|u| u.input), sum(|u| u.output), sum(|u| u.total_tokens)];
    let actual = [&model["input"], &model["output"], &model["totalTokens"]].map(|value| value.as_u64().unwrap());
    assert_eq!(actual, expected);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn records_usage_as_numeric_sets_on_the_ledger_in_the_entrys_commit() {
    let setup = setup();
    setup.faux.set_responses([answer("one"), answer("two")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    type UsageCommits = Arc<Mutex<Vec<(usize, Vec<Op>)>>>;
    let commits: UsageCommits = Arc::default();
    let sink = commits.clone();
    let unsubscribe = harness.observe_commits(commit_observer(move |publication, _| {
        for change in document_changes(publication) {
            if let DocumentCommitChange::Change { record, ops, .. } = change
                && record.kind == "pi.usage"
            {
                let entries =
                    publication.changes.iter().filter(|other| matches!(other, pi_durable::CommitChange::Entry(_)));
                sink.lock().unwrap().push((entries.count(), ops));
            }
        }
        Ok(())
    }));
    std::mem::forget(unsubscribe);
    root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    root.submit(SubmissionDraft::input("b"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let commits = commits.lock().unwrap().clone();
    assert_eq!(commits.len(), 2);
    let first: Vec<serde_json::Value> = commits[0].1.iter().map(|op| serde_json::to_value(op).unwrap()).collect();
    assert_eq!(first.len(), 1);
    assert_eq!((&first[0][0], &first[0][1]), (&json!("s"), &json!(["models", "faux/faux-1"])));
    assert!(first[0][2].is_object());
    for op in &commits[1].1 {
        let op = serde_json::to_value(op).unwrap();
        assert_eq!((&op[0], &op[1][1]), (&json!("s"), &json!("faux/faux-1")));
    }
    assert!(commits.iter().all(|(entries, _)| *entries >= 1));
    harness.close(&ctx()).await.unwrap();
}
