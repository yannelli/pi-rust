//! The upstream "compaction edge cases".

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use pi_durable::harness::compaction::CompactionDecision;
use pi_durable::harness::usage::USAGE_DOC;
use pi_durable::session::SessionEnd;

use super::chat::*;
use super::outcomes::before_compact;
use super::support::ControlledStorage;

#[tokio::test]
async fn keeps_the_one_compaction_limit_through_a_retry_backoff() {
    let chat = open(window(1000)).await;
    chat.history().await;
    chat.set_policy(blocking());
    let asked = Arc::new(AtomicUsize::new(0));
    let count = asked.clone();
    before_compact(&chat, "declines", move |_| {
        count.fetch_add(1, Ordering::SeqCst);
        Ok(Some(CompactionDecision::Decline))
    });
    chat.faux.agent(failure("overloaded"));
    chat.turn(&text("u4", 200), "a4").await;
    // One blocking compaction, declined; the retried preparation, still above the threshold, started no other.
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    assert_eq!(chat.faux.agent_requests().len(), 5);
    chat.close().await;
}

#[tokio::test]
async fn does_not_start_a_background_compaction_when_a_manual_one_was_admitted_during_preparation() {
    let chat = open(window(2000)).await;
    chat.history().await;
    chat.set_policy(background());
    let (rendering, release) = (Deferred::default(), Deferred::default());
    let hold = Arc::new(AtomicBool::new(true));
    let (signal, gate) = (rendering.clone(), release.clone());
    let slow = section(
        "slow",
        move |_, _| {
            let first = hold.swap(false, Ordering::SeqCst);
            let (signal, gate) = (signal.clone(), gate.clone());
            async move {
                if first {
                    signal.resolve();
                    gate.wait().await;
                }
                Ok(Some("slow".to_owned()))
            }
        },
        None,
    );
    chat.setup.registry.install(Extension::new("section:slow").section(slow)).unwrap();
    let reached = Deferred::default();
    chat.faux.summary(gated(&Deferred::default(), summary(), Some(&reached)));
    chat.faux.agent(answer("a4"));
    let input = chat.submit(&text("u4", 100)).await;
    rendering.wait().await;
    let manual = chat.compact(None).await;
    reached.wait().await;
    release.resolve();
    assert_eq!(input.wait(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    let ids: Vec<TaskId> = chat.compaction_tasks().await.iter().map(|task| task.id).collect();
    assert_eq!(ids, [manual]);
    // A background compaction would have sent its own summarization request.
    assert_eq!(chat.faux.summary_requests().len(), 1);
    chat.harness.abort_task(manual, &ctx()).await.unwrap();
    chat.close().await;
}

#[tokio::test]
async fn starts_no_background_compaction_after_a_blocking_one_in_the_same_generation() {
    let chat = open(window(2000)).await;
    chat.history().await;
    // Blocking at 1500, background at 200: the kept part stays above the background threshold.
    chat.set_policy(CompactionOverrides {
        background_tokens: Some(1300),
        keep_recent_tokens: Some(400),
        ..background()
    });
    chat.faux.summary(summary());
    chat.turn(&text("u4", 1000), "a4").await;
    assert_eq!(chat.faux.summary_requests().len(), 1);
    assert!(chat.compaction_tasks().await.is_empty());
    chat.close().await;
}

// Upstream's case where a blocking compaction faults patches `models.completeSimple` to throw. Rust
// `Models::complete_simple` returns a message and cannot fail, so this fault is not reachable from a test.

#[tokio::test]
async fn fails_an_overflow_run_with_its_text_when_its_compaction_is_aborted_directly_or_itself_overflows() {
    for abort in [true, false] {
        let chat = open(OpenOptions::default()).await;
        chat.history().await;
        chat.set_policy(CompactionOverrides { enabled: Some(true), ..manual() });
        let reached = Deferred::default();
        if abort {
            chat.faux.summary(gated(&Deferred::default(), summary(), Some(&reached)));
        } else {
            chat.faux.summary(failure("prompt is too long for the summary"));
        }
        chat.faux.agent(failure(OVERFLOW));
        let input = chat.submit(&text("u4", 100)).await;
        if abort {
            reached.wait().await;
            let child = chat.compaction_tasks().await.remove(0);
            chat.harness.abort_task(child.id, &ctx()).await.unwrap();
        }
        let settled = input.wait(&ctx()).await.unwrap();
        assert_eq!((settled.status, settled.reason.as_deref()), (SubmissionStatus::Unanswered, Some("model_error")));
        assert_eq!(settled.detail.map(|detail| detail.to_serde()), Some(json!(OVERFLOW)));
        assert!(chat.live().await.get("compactions").is_none());
        chat.close().await;
    }
}

#[tokio::test]
async fn treats_a_length_stop_as_an_ordinary_answer_not_an_overflow() {
    let chat = open(OpenOptions::default()).await;
    chat.history().await;
    chat.set_policy(CompactionOverrides { enabled: Some(true), ..manual() });
    let options = FauxMessageOptions {
        stop_reason: Some(StopReason::Length),
        error_message: Some(OVERFLOW.into()),
        ..FauxMessageOptions::default()
    };
    chat.faux.agent(faux_assistant_message("cut short", options));
    let input = chat.submit("go").await;
    assert_eq!(input.wait(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    assert!(chat.faux.summary_requests().is_empty());
    chat.close().await;
}

async fn until_event(events: &Events, kind: &'static str) {
    let seen = events.clone();
    wait_for(move || {
        let found = event_types(&seen).iter().any(|seen| seen == kind);
        async move { found }
    })
    .await;
}

#[tokio::test]
async fn orders_the_events_of_a_summary_placed_at_once() {
    let chat = open(OpenOptions::default()).await;
    chat.history().await;
    let (stream, events) = listen(&chat).await;
    chat.faux.summary(summary());
    let task = chat.compact(None).await;
    chat.result(task).await;
    until_event(&events, "compaction_end").await;
    let types = event_types(&events);
    let end = types.iter().position(|kind| kind == "compaction_end").unwrap();
    assert_eq!(
        types[end - 2..end + 3],
        ["message_start", "message_end", "compaction_end", "submission", "usage_changed"]
    );
    stream.stop().await;
    chat.close().await;
}

#[tokio::test]
async fn puts_compaction_start_last_in_the_batch_of_the_preparation_commit() {
    let chat = open(window(2000)).await;
    chat.history().await;
    chat.set_policy(background());
    let stream = watch_events(&chat.harness, chat.root.id, &ctx()).await.unwrap();
    let batches: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let sink = batches.clone();
    stream
        .start(move |batch, _| {
            let types = batch.iter().map(|event| Value::from(event)["type"].as_str().unwrap_or_default().to_owned());
            sink.lock().unwrap().push(types.collect());
            async { Ok(()) }
        })
        .unwrap();
    // A new section makes the preparation commit append a system entry next to the compaction's status.
    add_section(&chat.setup.registry, "extra", "extra", None);
    chat.faux.summary(gated(&Deferred::default(), summary(), None));
    chat.turn(&text("u4", 100), "a4").await;
    let has_start = |batch: &Vec<String>| batch.iter().any(|kind| kind == "compaction_start");
    let seen = batches.clone();
    wait_for(move || {
        let found = seen.lock().unwrap().iter().any(has_start);
        async move { found }
    })
    .await;
    let batch = batches.lock().unwrap().iter().find(|batch| has_start(batch)).cloned().unwrap();
    assert_eq!(batch, ["message_start", "message_end", "compaction_start"]);
    stream.stop().await;
    let task = chat.compaction_tasks().await.remove(0);
    chat.harness.abort_task(task.id, &ctx()).await.unwrap();
    chat.close().await;
}

#[tokio::test]
async fn keeps_a_background_summary_queued_through_a_retry_backoff_while_a_blocking_compaction_wins() {
    let chat = open(window(2000)).await;
    chat.history().await;
    chat.set_policy(background());
    let retry =
        RetryOverrides { enabled: Some(true), max_retries: Some(2), base_delay_ms: Some(300), ..Default::default() };
    chat.setup.settings.lock().unwrap().retry = Some(retry);
    let (summary_gate, summary_reached) = (Deferred::default(), Deferred::default());
    chat.faux.summary(gated(&summary_gate, answer("BACKGROUND"), Some(&summary_reached)));
    chat.faux.summary(answer("BLOCKING"));
    let failed = Deferred::default();
    let signal = failed.clone();
    chat.faux.agent(step(move |_| {
        signal.resolve();
        failure("overloaded")
    }));
    chat.faux.agent(answer("a4"));
    let input = chat.submit(&text("u4", 100)).await;
    failed.wait().await;
    summary_reached.wait().await;
    // During the backoff: the background summary queues, and the thresholds drop so the retry blocks.
    let probe = chat.clone();
    wait_for(move || {
        let chat = probe.clone();
        async move { !chat.live().await["generation"]["retry"].is_null() }
    })
    .await;
    let background_task = chat.compaction_tasks().await.remove(0);
    summary_gate.resolve();
    let submission = chat.placed(background_task.id).await;
    assert_eq!(submission.status(&ctx()).await.unwrap().status, SubmissionStatus::Queued);
    chat.set_policy(CompactionOverrides { reserve_tokens: Some(1500), keep_recent_tokens: Some(50), ..background() });
    assert_eq!(input.wait(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    assert!(first_user_text(&chat.faux.last_agent_request()).contains("BLOCKING"));
    let settled = submission.wait(&ctx()).await.unwrap();
    assert_eq!((settled.status, settled.reason.as_deref()), (SubmissionStatus::Unanswered, Some("stale")));
    chat.close().await;
}

#[tokio::test]
async fn summarizes_the_previous_summary_in_a_second_compaction() {
    let chat = open(OpenOptions::default()).await;
    chat.history().await;
    chat.faux.summary(answer("FIRST"));
    let first = chat.compact(None).await;
    chat.result(first).await;
    chat.turn(&text("u4", 100), &text("a4", 100)).await;
    chat.turn(&text("u5", 100), &text("a5", 100)).await;
    chat.faux.summary(answer("SECOND"));
    let second = chat.compact(None).await;
    chat.result(second).await;
    let prompt = user_text(chat.faux.summary_requests()[1].messages.get(1));
    assert!(prompt.starts_with("<conversation>\n[User]: The conversation history before this point was compacted"));
    assert!(prompt.contains("FIRST"));
    assert!(chat.first_text().await.contains("SECOND"));
    chat.close().await;
}

#[tokio::test]
async fn leaves_no_usage_submission_or_summary_when_the_classifying_commit_fails_which_fails_the_harness() {
    let storage = Arc::new(ControlledStorage::default());
    let chat = open(OpenOptions { storage: Some(storage.clone()), ..OpenOptions::default() }).await;
    chat.history().await;
    let usage = |chat: &Chat| {
        let (harness, id) = (chat.harness.clone(), chat.root.id);
        async move {
            harness.snapshot(&USAGE_DOC, id, &ctx()).await.unwrap().unwrap().to_serde()["models"]["faux/faux-1"].clone()
        }
    };
    let before = usage(&chat).await;
    let store = storage.clone();
    chat.faux.summary(step(move |_| {
        store.fail_next_commit(pi_durable::DurableError::error("disk gone"));
        summary()
    }));
    let task = chat.compact(None).await;
    let SessionEnd::Failed(error) = chat.harness.closed().await else { panic!("the Harness must fail") };
    assert_eq!(error.to_string(), "disk gone");
    chat.close().await;
    // Nothing of the failed commit landed: the task is still live, without usage, entry, or submission.
    storage.reopen();
    let request = format!("compaction:{task}");
    assert!(storage.submission_by_request(chat.root.id, &request, &ctx()).await.unwrap().is_none());
    let record = storage.task(task, &ctx()).await.unwrap().unwrap();
    assert_ne!(record.state.status(), pi_durable::TaskStatus::Terminal);
    let (harness, root) = open_chat(storage.clone(), &chat.setup).await;
    let reopened = Chat { harness, root, setup: chat.setup.clone(), faux: chat.faux.clone() };
    assert_eq!(Json::from(usage(&reopened).await), Json::from(before));
    assert!(!reopened.kinds().await.contains(&"pi.compaction".to_owned()));
    reopened.close().await;
}
