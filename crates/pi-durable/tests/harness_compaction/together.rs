//! The upstream cases of blocking and manual compaction together, pinning and late policy changes, blocked
//! compaction, and context contributions.

use pi_durable::harness::compaction::compaction_task;
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::harness::usage::USAGE_DOC;
use pi_durable::session::Tx;
use pi_durable::storage::open_sqlite_storage;
use pi_durable::tasks::{TaskOptions, TaskTemplate};
use pi_durable::{ContextEdit, DraftHead, EntryDraft};

use super::chat::*;

struct BlockingRun {
    input: Submission,
    gate: Deferred,
    blocking: TaskId,
}

/// A run whose generation waits on a blocking compaction whose summary is held.
async fn blocking_run(chat: &Chat) -> BlockingRun {
    chat.history().await;
    chat.set_policy(blocking());
    let (gate, reached) = (Deferred::default(), Deferred::default());
    chat.faux.summary(gated(&gate, answer("BLOCKING"), Some(&reached)));
    let input = chat.submit(&text("u4", 200)).await;
    reached.wait().await;
    let blocking = chat.compaction_tasks().await.remove(0).id;
    BlockingRun { input, gate, blocking }
}

#[tokio::test]
async fn places_a_manual_summary_selected_before_the_blocking_one_landed_its_equal_cut_replaces_it() {
    let chat = open(window(1000)).await;
    let run = blocking_run(&chat).await;
    // Selected from the same context as the blocking compaction, so it cuts at the same entry.
    chat.faux.summary(answer("MANUAL"));
    let manual = chat.compact(None).await;
    let submission = chat.placed(manual).await;
    assert_eq!(submission.status(&ctx()).await.unwrap().status, SubmissionStatus::Queued);
    chat.faux.agent(answer("a4"));
    run.gate.resolve();
    assert_eq!(run.input.wait(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    // The request after the blocking compaction used its summary; the manual one landed at the final boundary.
    assert!(first_user_text(&chat.faux.last_agent_request()).contains("BLOCKING"));
    assert_eq!(submission.wait(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    let markers: Vec<EntryRecord> =
        all_entries(&chat.root).await.into_iter().filter(|record| record.kind == "pi.compaction").collect();
    assert_eq!(markers.len(), 2);
    assert_eq!(markers[1].head, markers[0].head);
    let messages = chat.root.context(&ctx(), None).await.unwrap().messages;
    assert!(first_user_text(&messages).contains("MANUAL"));
    assert!(!messages.iter().any(|message| user_text(Some(message)).contains("BLOCKING")));
    chat.close().await;
}

#[tokio::test]
async fn finds_nothing_to_compact_for_a_manual_compaction_selected_after_the_blocking_summary_landed() {
    let chat = open(window(1000)).await;
    let run = blocking_run(&chat).await;
    let (answer_gate, answer_reached) = (Deferred::default(), Deferred::default());
    chat.faux.agent(gated(&answer_gate, answer("a4"), Some(&answer_reached)));
    run.gate.resolve();
    answer_reached.wait().await;
    let manual = chat.compact(None).await;
    assert_eq!(chat.outcome(manual).await, json!({ "status": "completed", "result": {} }));
    assert_eq!(chat.faux.summary_requests().len(), 1);
    answer_gate.resolve();
    assert_eq!(run.input.wait(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    chat.close().await;
}

#[tokio::test]
async fn aborts_the_run_and_both_compactions_on_esc_and_appends_nothing() {
    let chat = open(window(1000)).await;
    let run = blocking_run(&chat).await;
    let manual_reached = Deferred::default();
    chat.faux.summary(gated(&Deferred::default(), answer("MANUAL"), Some(&manual_reached)));
    let manual = chat.compact(None).await;
    manual_reached.wait().await;
    let blocking: Vec<Value> =
        chat.live().await["compactions"].as_array().unwrap().iter().map(|status| status["blocking"].clone()).collect();
    assert_eq!(blocking, [json!(true), json!(false)]);
    chat.root.abort(&ctx(), false).await.unwrap();
    let settled = run.input.wait(&ctx()).await.unwrap();
    assert_eq!((settled.status, settled.reason.as_deref()), (SubmissionStatus::Unanswered, Some("aborted")));
    assert_eq!(chat.outcome(run.blocking).await["status"], "aborted");
    assert_eq!(chat.outcome(manual).await["status"], "aborted");
    let live = chat.live().await;
    assert!(live.get("compactions").is_none() && live.get("run").is_none());
    assert!(!chat.kinds().await.contains(&"pi.compaction".to_owned()));
    assert!(chat.harness.inspect(&ctx()).await.unwrap().submissions.is_empty());
    chat.close().await;
}

#[tokio::test]
async fn places_a_summary_that_survived_esc_at_the_next_submission_before_its_input() {
    let chat = open(OpenOptions::default()).await;
    chat.history().await;
    let reached = Deferred::default();
    chat.faux.agent(gated(&Deferred::default(), answer("never"), Some(&reached)));
    let busy = chat.submit("busy").await;
    reached.wait().await;
    chat.faux.summary(summary());
    let task = chat.compact(None).await;
    let submission = chat.placed(task).await;
    chat.root.abort(&ctx(), false).await.unwrap();
    let settled = busy.wait(&ctx()).await.unwrap();
    assert_eq!((settled.status, settled.reason.as_deref()), (SubmissionStatus::Unanswered, Some("aborted")));
    assert_eq!(submission.status(&ctx()).await.unwrap().status, SubmissionStatus::Queued);
    chat.turn("u5", "a5").await;
    assert_eq!(submission.status(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    let request = chat.faux.last_agent_request();
    assert!(first_user_text(&request).contains("SUMMARY"));
    assert!(request.iter().any(|message| user_text(Some(message)) == "u5"));
    assert_eq!(tail(&chat.kinds().await, 4), ["pi.compaction", "pi.user", "pi.system", "pi.assistant"]);
    chat.close().await;
}

#[tokio::test]
async fn keeps_the_pinned_model_through_a_retry_and_advances_the_live_attempt() {
    let model = |id: &str| FauxModelDefinition {
        context_window: Some(100_000),
        max_tokens: Some(900),
        ..FauxModelDefinition::new(id)
    };
    let models = vec![model("faux-1"), model("faux-2")];
    let setup = chat_setup(RegisterFauxProviderOptions { models, ..Default::default() });
    let faux = script(&setup);
    let chat = open_with(OpenOptions::default(), setup, faux).await;
    chat.history().await;
    let attempt = Arc::new(Mutex::new(None));
    let root = chat.root.clone();
    chat.faux.summary(Step::Run(Arc::new(move |_| {
        let root = root.clone();
        Box::pin(async move {
            // Switched while the request runs: the retry still uses the pinned model.
            root.configure(AgentChange::model(ModelRef::new("faux", "faux-2")), &ctx()).await.unwrap();
            Ok(failure("overloaded"))
        })
    })));
    let (probe, sink) = (chat.clone(), attempt.clone());
    chat.faux.summary(Step::Run(Arc::new(move |_| {
        let (chat, sink) = (probe.clone(), sink.clone());
        Box::pin(async move {
            *sink.lock().unwrap() = chat.live().await["compactions"][0]["attempt"].as_u64();
            Ok(summary())
        })
    })));
    let task = chat.compact(None).await;
    assert_eq!(chat.outcome(task).await["status"], "completed");
    let models: Vec<String> = chat.faux.summary_requests().into_iter().map(|request| request.model).collect();
    assert_eq!(models, ["faux-1", "faux-1"]);
    assert_eq!(*attempt.lock().unwrap(), Some(2));
    let usage = chat.harness.snapshot(&USAGE_DOC, chat.root.id, &ctx()).await.unwrap().unwrap().to_serde();
    let keys: Vec<&String> = usage["models"].as_object().unwrap().keys().collect();
    assert_eq!(keys, ["faux/faux-1"]);
    chat.close().await;
}

#[tokio::test]
async fn shows_a_late_joiner_a_compaction_that_is_summarizing() {
    let chat = open(OpenOptions::default()).await;
    chat.history().await;
    let reached = Deferred::default();
    chat.faux.summary(gated(&Deferred::default(), summary(), Some(&reached)));
    let task = chat.compact(None).await;
    reached.wait().await;
    let expected = json!([{ "taskId": task, "trigger": "manual", "blocking": false, "attempt": 1 }]);
    assert_eq!(watched_live(&chat).await["compactions"], expected);
    chat.harness.abort_task(task, &ctx()).await.unwrap();
    chat.close().await;
}

// Upstream's two cases that treat silent overflow as an ordinary answer set `reserveTokens` to -100000 to put the
// thresholds out of reach. Rust compaction settings hold token counts as `u64`, so that policy is not expressible.

#[tokio::test]
async fn sends_the_request_when_a_blocking_compaction_finds_nothing_under_a_policy_changed_after_preparation() {
    let chat = open(window(1000)).await;
    chat.history().await;
    chat.set_policy(blocking());
    let changed = Arc::new(Mutex::new(false));
    let settings = chat.setup.settings.clone();
    let policy = section(
        "policy",
        move |_, _| {
            // Rendering runs after preparation read the policy; the compaction reads this one.
            if !std::mem::replace(&mut *changed.lock().unwrap(), true) {
                settings.lock().unwrap().compaction =
                    Some(CompactionOverrides { keep_recent_tokens: Some(100_000), ..blocking() });
            }
            std::future::ready(Ok(Some("p".to_owned())))
        },
        None,
    );
    chat.setup.registry.install(Extension::new("section:policy").section(policy)).unwrap();
    chat.turn(&text("u4", 200), "a4").await;
    assert!(chat.compaction_tasks().await.is_empty());
    assert!(chat.faux.summary_requests().is_empty());
    assert!(!chat.kinds().await.contains(&"pi.compaction".to_owned()));
    chat.close().await;
}

/// The compaction task as a newer process would have stored it.
struct Newer(Task);

impl TaskTemplate for Newer {
    fn name(&self) -> &str {
        self.0.name()
    }

    fn version(&self) -> u32 {
        2
    }

    fn initial(&self, input: &Json) -> pi_durable::DurableResult<Json> {
        self.0.initial(input)
    }
}

#[tokio::test]
async fn survives_reopen_blocked_and_is_orphaned_on_abort_with_its_status_removed() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite").to_string_lossy().into_owned();
    let setup = chat_setup(Default::default());
    let (harness, root) = open_chat(Arc::new(open_sqlite_storage(&path).await.unwrap()), &setup).await;
    // A compaction stored by a newer version than this process registers, for example after a downgrade.
    let id = root.id;
    let create = move |tx: Tx| async move {
        let newer: Arc<dyn TaskTemplate> = Arc::new(Newer(compaction_task()));
        let input = Json::from(json!({ "reason": "manual" }));
        let options = TaskOptions { conversation_id: None, ..TaskOptions::conversation(id) };
        let task = tx.create_task(newer, input, options).await?;
        let status = json!([{ "taskId": task, "trigger": "manual", "blocking": false, "attempt": 1 }]);
        tx.doc(&LIVE_DOC, id).await?.update(|live| live.set("compactions", Json::from(status)))??;
        Ok(task)
    };
    let task = root.commit(create, &ctx()).await.unwrap();
    harness.close(&ctx()).await.unwrap();
    let faux = script(&setup);
    let (harness, root) = open_chat(Arc::new(open_sqlite_storage(&path).await.unwrap()), &setup).await;
    harness.resume().unwrap();
    let chat = Chat { harness, root, setup: Arc::new(setup), faux };
    let inspection = chat.harness.inspect(&ctx()).await.unwrap();
    let state = inspection.tasks.iter().find(|candidate| candidate.record.id == task).unwrap().state.clone();
    assert!(matches!(state, TaskInspectionState::Blocked { reason: BlockedReason::TaskTooOld, .. }), "{state:?}");
    chat.harness.abort_task(task, &ctx()).await.unwrap();
    let outcome = chat.harness.wait_for_task(task, &ctx()).await.unwrap().state;
    assert_eq!(
        serde_json::to_value(outcome.outcome()).unwrap(),
        json!({ "status": "orphaned", "reason": "task_too_old" })
    );
    assert!(chat.live().await.get("compactions").is_none());
    let failure = json!({ "taskId": task, "trigger": "manual", "error": "task_too_old" });
    assert_eq!(chat.live().await["compactionFailure"], failure);
    chat.close().await;
}

fn note(kind: &str, text: &str) -> EntryDraft {
    let message = Message::User(pi_ai::UserMessage { content: pi_ai::UserContent::Text(text.into()), timestamp: 0 });
    EntryDraft { model: Some(vec![message]), ..EntryDraft::new(kind) }
}

#[tokio::test]
async fn apply_edits_carried_by_an_older_head_marker_in_the_range() {
    let chat = open(OpenOptions::default()).await;
    let id = chat.root.id;
    let write = move |tx: Tx| async move {
        let a = tx.append_entry(id, note("app.note", "a")).await?.id;
        let b = tx.append_entry(id, note("app.note", "b")).await?.id;
        // An older marker that omits b, then a newer one whose range still contains the older marker.
        let older = EntryDraft {
            head: Some(DraftHead::Entry(a)),
            edits: Some(vec![ContextEdit::omit(b)]),
            ..EntryDraft::new("app.head")
        };
        tx.append_entry(id, older).await?;
        let c = tx.append_entry(id, note("app.note", "c")).await?.id;
        tx.append_entry(id, EntryDraft { head: Some(DraftHead::Entry(a)), ..note("app.head", "H") }).await?;
        Ok([a, b, c])
    };
    let ids = chat.root.commit(write, &ctx()).await.unwrap();
    let view = chat.root.context(&ctx(), None).await.unwrap();
    let viewed: Vec<_> = view.entries[1..].iter().map(|record| record.id).collect();
    assert_eq!(viewed, ids);
    let contributions: Vec<Vec<String>> = view
        .contributions
        .iter()
        .map(|messages| messages.iter().map(|message| user_text(Some(message))).collect())
        .collect();
    assert_eq!(contributions, [vec!["H"], vec!["a"], vec![], vec!["c"]]);
    let messages: Vec<String> = view.messages.iter().map(|message| user_text(Some(message))).collect();
    assert_eq!(messages, ["H", "a", "c"]);
    // The summarizer sees the same contributions: the omitted entry stays out.
    chat.set_policy(CompactionOverrides { keep_recent_tokens: Some(1), ..manual() });
    chat.faux.summary(summary());
    let task = chat.compact(None).await;
    chat.result(task).await;
    let prompt = user_text(chat.faux.summary_requests()[0].messages.get(1));
    assert!(prompt.contains("<conversation>\n[User]: H\n\n[User]: a\n</conversation>"), "{prompt}");
    chat.close().await;
}
