//! The upstream "compaction recovery" cases, over SQLite storage that outlives each Harness.

use std::sync::atomic::{AtomicUsize, Ordering};

use pi_durable::harness::compaction::{CompactionHooks, compaction_task};
use pi_durable::harness::usage::USAGE_DOC;
use pi_durable::storage::open_sqlite_storage;

use super::chat::*;
use super::support::task_support::aborted;

async fn sqlite(path: &str) -> Arc<dyn Storage> {
    Arc::new(open_sqlite_storage(path).await.unwrap())
}

async fn reopen(path: &str, chat: &Chat) -> Chat {
    let (harness, root) = open_chat(sqlite(path).await, &chat.setup).await;
    harness.resume().unwrap();
    Chat { harness, root, setup: chat.setup.clone(), faux: chat.faux.clone() }
}

/// A chat over SQLite at `path` with three turns of history.
async fn first(path: &str) -> Chat {
    let setup = chat_setup(Default::default());
    add_section(&setup.registry, "preamble", "You are helpful.", Some(false));
    let faux = script(&setup);
    let (harness, root) = open_chat(sqlite(path).await, &setup).await;
    {
        let mut settings = setup.settings.lock().unwrap();
        settings.compaction = Some(manual());
        settings.retry = Some(RetryOverrides {
            enabled: Some(true),
            max_retries: Some(2),
            base_delay_ms: Some(1),
            ..Default::default()
        });
    }
    harness.resume().unwrap();
    let chat = Chat { harness, root, setup: Arc::new(setup), faux };
    chat.history().await;
    chat
}

fn temp_path() -> (tempfile::TempDir, String) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite").to_string_lossy().into_owned();
    (directory, path)
}

async fn usage(chat: &Chat) -> Value {
    chat.harness.snapshot(&USAGE_DOC, chat.root.id, &ctx()).await.unwrap().unwrap().to_serde()
}

async fn checkpoint(chat: &Chat, id: TaskId) -> Value {
    let state = chat.harness.get_task(id, &ctx()).await.unwrap().unwrap().state;
    serde_json::to_value(state).unwrap()["checkpoint"].clone()
}

#[tokio::test]
async fn repeats_nothing_after_reopen_once_the_summary_is_placed() {
    let (_directory, path) = temp_path();
    let chat = first(&path).await;
    chat.faux.summary(summary());
    let task = chat.compact(None).await;
    chat.result(task).await;
    let entries = all_entries(&chat.root).await;
    let before = usage(&chat).await;
    chat.close().await;
    let chat = reopen(&path, &chat).await;
    chat.harness.wait_for_idle(&ctx()).await.unwrap();
    assert_eq!(chat.faux.summary_requests().len(), 1);
    assert_eq!(all_entries(&chat.root).await, entries);
    assert_eq!(Json::from(usage(&chat).await), Json::from(before));
    let inspection = chat.harness.inspect(&ctx()).await.unwrap();
    assert!(inspection.tasks.is_empty() && inspection.submissions.is_empty());
    chat.close().await;
}

#[tokio::test]
async fn fails_an_overflow_run_with_its_text_when_its_compaction_fails_after_reopen() {
    let (_directory, path) = temp_path();
    let chat = first(&path).await;
    chat.set_policy(CompactionOverrides { enabled: Some(true), ..manual() });
    let reached = Deferred::default();
    chat.faux.summary(gated(&Deferred::default(), summary(), Some(&reached)));
    chat.faux.agent(failure(OVERFLOW));
    let input = chat.submit(&text("u4", 100)).await;
    reached.wait().await;
    let generation = chat.run_task().await;
    let state = serde_json::to_value(chat.harness.get_task(generation, &ctx()).await.unwrap().unwrap().state).unwrap();
    assert_eq!(state["status"], "waiting");
    assert_eq!(
        (&state["checkpoint"]["phase"], &state["checkpoint"]["overflow"]),
        (&json!("prepare"), &json!(OVERFLOW))
    );
    chat.close().await;
    let chat = reopen(&path, &chat).await;
    chat.faux.summary(failure("bad request"));
    let settled = chat.harness.submission(input.id, &ctx()).await.unwrap().unwrap().wait(&ctx()).await.unwrap();
    assert_eq!((settled.status, settled.reason.as_deref()), (SubmissionStatus::Unanswered, Some("model_error")));
    assert_eq!(settled.detail.map(|detail| detail.to_serde()), Some(json!(OVERFLOW)));
    chat.close().await;
}

#[tokio::test]
async fn reruns_select_and_its_hook_after_a_crash_in_select() {
    let (_directory, path) = temp_path();
    let chat = first(&path).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let reached = Deferred::default();
    let (count, signal) = (calls.clone(), reached.clone());
    let hooks = CompactionHooks {
        before_compact: Some(Arc::new(move |_, _, hook_context| {
            let (first, signal) = (count.fetch_add(1, Ordering::SeqCst) == 0, signal.clone());
            Box::pin(async move {
                if first {
                    signal.resolve();
                    aborted(hook_context.abort_signal().unwrap()).await;
                }
                Ok(None)
            })
        })),
    };
    chat.setup.registry.install(Extension::new("hooks").hook(hook(&compaction_task(), hooks))).unwrap();
    let task = chat.compact(None).await;
    reached.wait().await;
    chat.close().await;
    let chat = reopen(&path, &chat).await;
    chat.faux.summary(summary());
    assert_eq!(chat.outcome(task).await["status"], "completed");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    chat.close().await;
}

fn shape(messages: &[Message]) -> Vec<(&'static str, String)> {
    roles(messages).into_iter().zip(messages.iter().map(|message| user_text(Some(message)))).collect()
}

#[tokio::test]
async fn resends_an_interrupted_summarization_once_and_counts_only_the_answered_attempt() {
    let (_directory, path) = temp_path();
    let chat = first(&path).await;
    let reached = Deferred::default();
    chat.faux.summary(gated(&Deferred::default(), summary(), Some(&reached)));
    let task = chat.compact(None).await;
    reached.wait().await;
    let before = usage(&chat).await["models"]["faux/faux-1"].clone();
    assert_eq!(checkpoint(&chat, task).await["phase"], "summarize");
    chat.close().await;
    let chat = reopen(&path, &chat).await;
    chat.faux.summary(summary());
    assert_eq!(chat.outcome(task).await["status"], "completed");
    let requests = chat.faux.summary_requests();
    assert_eq!(requests.len(), 2);
    // The resent request is the pinned one; only timestamps may differ.
    assert_eq!(shape(&requests[1].messages), shape(&requests[0].messages));
    let after = usage(&chat).await["models"]["faux/faux-1"].clone();
    assert!(after["input"].as_u64() > before["input"].as_u64());
    assert_eq!(after["output"].as_u64().unwrap() - before["output"].as_u64().unwrap(), 2);
    chat.close().await;
}

#[tokio::test]
async fn resumes_a_retry_backoff_after_reopen() {
    let (_directory, path) = temp_path();
    let chat = first(&path).await;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as f64;
    *chat.setup.now.lock().unwrap() = Some(now);
    let retry =
        RetryOverrides { enabled: Some(true), max_retries: Some(2), base_delay_ms: Some(60_000), ..Default::default() };
    chat.setup.settings.lock().unwrap().retry = Some(retry);
    chat.faux.summary(failure("overloaded"));
    let task = chat.compact(None).await;
    let probe = chat.clone();
    wait_for(move || {
        let chat = probe.clone();
        async move { !chat.live().await["compactions"][0]["retry"].is_null() }
    })
    .await;
    let state = checkpoint(&chat, task).await;
    assert_eq!((&state["phase"], &state["attempt"]), (&json!("retry"), &json!(1)));
    chat.close().await;
    *chat.setup.now.lock().unwrap() = Some(now + 120_000.0);
    let chat = reopen(&path, &chat).await;
    chat.faux.summary(summary());
    assert_eq!(chat.outcome(task).await["status"], "completed");
    assert_eq!(chat.faux.summary_requests().len(), 2);
    chat.close().await;
}

#[tokio::test]
async fn keeps_a_generation_waiting_on_its_blocking_compaction_across_reopen() {
    let (_directory, path) = temp_path();
    let chat = first(&path).await;
    let reached = Deferred::default();
    chat.faux.summary(gated(&Deferred::default(), summary(), Some(&reached)));
    // The faux model window is 128k; this blocking threshold needs a small context window.
    chat.set_policy(CompactionOverrides { reserve_tokens: Some(128_000 - 700), ..blocking() });
    let input = chat.submit(&text("u4", 200)).await;
    reached.wait().await;
    chat.close().await;
    let chat = reopen(&path, &chat).await;
    chat.faux.summary(summary());
    chat.faux.agent(answer("a4"));
    let settled = chat.harness.submission(input.id, &ctx()).await.unwrap().unwrap().wait(&ctx()).await.unwrap();
    assert_eq!(settled.status, SubmissionStatus::Done);
    assert_eq!(tail(&chat.kinds().await, 3), ["pi.compaction", "pi.system", "pi.assistant"]);
    chat.close().await;
}

#[tokio::test]
async fn keeps_a_queued_summary_across_reopen_and_places_it_at_the_next_boundary() {
    let (_directory, path) = temp_path();
    let chat = first(&path).await;
    let reached = Deferred::default();
    chat.faux.agent(gated(&Deferred::default(), answer("never"), Some(&reached)));
    let input = chat.submit("busy").await;
    reached.wait().await;
    chat.faux.summary(summary());
    let task = chat.compact(None).await;
    let placed = chat.placed(task).await.id;
    chat.close().await;
    let chat = reopen(&path, &chat).await;
    chat.faux.agent(answer("answered"));
    let settled = chat.harness.submission(input.id, &ctx()).await.unwrap().unwrap().wait(&ctx()).await.unwrap();
    assert_eq!(settled.status, SubmissionStatus::Done);
    let summary = chat.harness.submission(placed, &ctx()).await.unwrap().unwrap().wait(&ctx()).await.unwrap();
    assert_eq!(summary.status, SubmissionStatus::Done);
    chat.close().await;
}
