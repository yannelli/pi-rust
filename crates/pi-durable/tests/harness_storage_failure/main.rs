//! Port of upstream `test/harness-storage-failure.test.ts`. This file holds the failed Storage call cases;
//! `listeners.rs` holds the listener and report error cases.

#[path = "../support/mod.rs"]
mod support;

mod listeners;
mod requests;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use pi_chord::Json;
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::harness::*;
use pi_durable::models::create_models;
use pi_durable::session::{SessionEnd, Tx, WatchEnd};
use pi_durable::tasks::TaskOptions;
use pi_durable::*;
use serde_json::json;
use support::task_support::{aborted, open_tasks};
use support::{ControlledStorage, Deferred, StorageCall, context};

/// Waits for its signal, records that it saw it, and ends `aborted` through its abort handler.
pub fn waiting(signals: &Arc<Mutex<Vec<&'static str>>>) -> Task {
    let sink = signals.clone();
    define_task(
        TaskDefinition::new("test.waiting", 1, |_| Ok(Json::from(json!({ "phase": "work" }))))
            .phase("work", move |_, runtime, _| {
                let sink = sink.clone();
                async move {
                    aborted(runtime.signal()).await;
                    sink.lock().unwrap().push("aborted");
                    Ok(())
                }
            })
            .abort(|_, runtime, context| async move {
                let state = TaskState::Terminal { outcome: TaskOutcome::Aborted { reason: None, result: None } };
                runtime.commit(move |_, _| async move { Ok(Some(state)) }, &context).await
            }),
    )
}

pub struct Started {
    pub harness: Harness,
    pub root: Conversation,
    pub reports: Arc<Mutex<Vec<DurableError>>>,
    pub signals: Arc<Mutex<Vec<&'static str>>>,
}

pub async fn started(storage: Arc<dyn Storage>) -> Started {
    let signals = Arc::new(Mutex::new(Vec::new()));
    let opened = open_tasks(storage, vec![waiting(&signals)]).await;
    let root = opened.harness.root(&context(), CreateOptions::default()).await.unwrap();
    Started { harness: opened.harness, root, reports: opened.reports, signals }
}

pub async fn note(root: &Conversation) -> DurableResult<EntryId> {
    let id = root.id;
    root.commit(move |tx: Tx| async move { Ok(tx.append_entry(id, EntryDraft::new("note")).await?.id) }, &context())
        .await
}

pub fn messages(reports: &Mutex<Vec<DurableError>>) -> Vec<String> {
    reports.lock().unwrap().iter().map(ToString::to_string).collect()
}

fn failed_by(error: &DurableError, cause: &str) -> bool {
    error.name() == "SessionFailed" && error.session_failure_cause().is_some_and(|inner| inner.to_string() == cause)
}

async fn closed_reason(harness: &Harness) -> (String, Option<String>) {
    match harness.closed().await {
        SessionEnd::Closed => ("closed".into(), None),
        SessionEnd::Failed(error) => ("failed".into(), Some(error.to_string())),
    }
}

/// Fail each `(method, first range page)` call named in `failing` once, with "disk gone".
fn fail_once(storage: &ControlledStorage, failing: Arc<Mutex<Vec<&'static str>>>, held: Option<(Deferred, Deferred)>) {
    let hook = move |call: StorageCall| {
        let range = call.method == "scan_entries" && call.limit > 1;
        let name = if range { "scanRange" } else { call.method };
        let held = held.clone().filter(|_| range);
        let failing = failing.clone();
        Box::pin(async move {
            if let Some((entered, release)) = held {
                entered.resolve();
                release.wait().await;
            }
            let mut failing = failing.lock().unwrap();
            match failing.iter().position(|method| *method == name) {
                Some(index) => {
                    failing.remove(index);
                    Err(DurableError::storage("disk gone"))
                }
                None => Ok(()),
            }
        }) as futures::future::BoxFuture<'static, DurableResult<()>>
    };
    *storage.call_hook.lock().unwrap() = Some(Arc::new(hook));
}

#[tokio::test]
async fn ends_everything_with_the_first_error_the_call_later_calls_waits_watches_streams_running_tasks() {
    let storage = Arc::new(ControlledStorage::default());
    let Started { harness, root, reports, signals } = started(storage.clone()).await;
    let task = waiting(&signals);
    let id = root.id;
    let id = root
        .commit(
            move |tx: Tx| async move { tx.create_task(task, Json::Null, TaskOptions::conversation(id)).await },
            &context(),
        )
        .await
        .unwrap();
    harness.resume().unwrap();
    while harness.get_task(id, &context()).await.unwrap().unwrap().state.status() != TaskStatus::Running {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let waiter = harness.clone();
    let task = tokio::spawn(async move { waiter.wait_for_task(id, &context()).await.map(|_| ()) });
    let waiter = harness.clone();
    let idle = tokio::spawn(async move { waiter.wait_for_idle(&context()).await });
    let watch = harness.watch_doc(&LIVE_DOC, root.id, &context()).await.unwrap().unwrap();
    let stream = root.watch(&context(), None).await.unwrap();
    stream.start(|_, _| async { Ok(()) }).unwrap();

    storage.fail_next_commit(DurableError::storage("disk gone"));
    // The call that hit it gets the storage error itself.
    assert_eq!(note(&root).await.unwrap_err().to_string(), "disk gone");
    assert!(failed_by(&task.await.unwrap().unwrap_err(), "disk gone"));
    assert!(failed_by(&idle.await.unwrap().unwrap_err(), "disk gone"));
    for end in [watch.closed().await, stream.closed().await] {
        assert!(matches!(&end, WatchEnd::Failed(error) if error.to_string() == "disk gone"), "{end:?}");
    }
    // Every later call, and a wait that starts now.
    assert!(failed_by(&harness.get_task(id, &context()).await.unwrap_err(), "disk gone"));
    assert!(failed_by(&harness.wait_for_idle(&context()).await.unwrap_err(), "disk gone"));
    assert!(failed_by(
        &root.submit(SubmissionDraft::input("x"), &context()).await.map(|_| ()).unwrap_err(),
        "disk gone"
    ));
    assert_eq!(closed_reason(&harness).await, ("failed".into(), Some("disk gone".into())));
    assert_eq!(*signals.lock().unwrap(), ["aborted"]);
    assert_eq!(messages(&reports), ["disk gone"]);
    harness.close(&context()).await.ok();
}

#[tokio::test]
async fn fails_it_from_a_read_off_the_session_line_as_a_context_read_makes() {
    let storage = Arc::new(ControlledStorage::default());
    let Started { harness, root, reports, .. } = started(storage.clone()).await;
    note(&root).await.unwrap();
    fail_once(&storage, Arc::new(Mutex::new(vec!["scanRange"])), None);
    assert_eq!(root.context(&context(), None).await.map(|_| ()).unwrap_err().to_string(), "disk gone");
    assert_eq!(closed_reason(&harness).await, ("failed".into(), Some("disk gone".into())));
    assert_eq!(messages(&reports), ["disk gone"]);
    harness.close(&context()).await.ok();
}

#[tokio::test]
async fn ends_storage_calls_underway_with_session_failed_and_closes_once_they_have_settled() {
    let storage = Arc::new(ControlledStorage::default());
    let Started { harness, root, .. } = started(storage.clone()).await;
    note(&root).await.unwrap();
    let failing = Arc::new(Mutex::new(Vec::new()));
    let (entered, release) = (Deferred::default(), Deferred::default());
    fail_once(&storage, failing.clone(), Some((entered.clone(), release.clone())));
    let reader = root.clone();
    let reading = tokio::spawn(async move { reader.context(&context(), None).await.map(|_| ()) });
    entered.wait().await;
    // Another call fails the Session while the context read is underway.
    failing.lock().unwrap().push("mint_id");
    assert_eq!(note(&root).await.unwrap_err().to_string(), "disk gone");
    let closed = Arc::new(AtomicBool::new(false));
    let (flag, watcher) = (closed.clone(), harness.clone());
    tokio::spawn(async move {
        watcher.closed().await;
        flag.store(true, Ordering::SeqCst);
    });
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(!closed.load(Ordering::SeqCst));
    // The read succeeds at the backend, yet its caller gets the failure: nothing it read is used.
    release.resolve();
    assert!(failed_by(&reading.await.unwrap().unwrap_err(), "disk gone"));
    assert_eq!(closed_reason(&harness).await, ("failed".into(), Some("disk gone".into())));
    harness.close(&context()).await.ok();
}

#[tokio::test]
async fn closes_the_backend_only_after_a_paged_read_underway_which_cannot_start_another_page() {
    let storage = Arc::new(ControlledStorage::default());
    let pages = Arc::new(Mutex::new(Vec::<&'static str>::new()));
    let (first_page, release_first) = (Deferred::default(), Deferred::default());
    let Started { harness, root, .. } = started(storage.clone()).await;
    // More entries than one page of the context read.
    let id = root.id;
    let fill = move |tx: Tx| async move {
        for _ in 0..300 {
            tx.append_entry(id, EntryDraft::new("note")).await?;
        }
        Ok(())
    };
    root.commit(fill, &context()).await.unwrap();
    let (sink, entered, release) = (pages.clone(), first_page.clone(), release_first.clone());
    *storage.call_hook.lock().unwrap() = Some(Arc::new(move |call: StorageCall| {
        let (sink, entered, release) = (sink.clone(), entered.clone(), release.clone());
        Box::pin(async move {
            if call.method == "close" {
                sink.lock().unwrap().push("close");
            } else if call.method == "scan_entries" && call.limit > 1 {
                sink.lock().unwrap().push(if call.first_page { "first" } else { "next" });
                if call.first_page {
                    entered.resolve();
                    release.wait().await;
                }
            }
            Ok(())
        })
    }));
    let reader = root.clone();
    let reading = tokio::spawn(async move { reader.context(&context(), None).await.map(|_| ()) });
    first_page.wait().await;
    let closer = harness.clone();
    let closing = tokio::spawn(async move { closer.close(&context()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    release_first.resolve();
    let error = reading.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("is closed"), "{error}");
    closing.await.unwrap().unwrap();
    assert_eq!(*pages.lock().unwrap(), ["first", "close"]);
    assert_eq!(closed_reason(&harness).await, ("closed".into(), None));
}

#[tokio::test]
async fn fails_it_when_storage_cannot_close_an_earlier_failure_stays_the_cause() {
    let storage = Arc::new(ControlledStorage::default());
    *storage.close_failure.lock().unwrap() = Some(DurableError::storage("close failed"));
    let healthy = started(storage).await;
    assert_eq!(healthy.harness.close(&context()).await.unwrap_err().to_string(), "close failed");
    assert_eq!(closed_reason(&healthy.harness).await, ("failed".into(), Some("close failed".into())));
    assert_eq!(messages(&healthy.reports), ["close failed"]);

    let storage = Arc::new(ControlledStorage::default());
    *storage.close_failure.lock().unwrap() = Some(DurableError::storage("close failed"));
    let failed = started(storage.clone()).await;
    storage.fail_next_commit(DurableError::storage("disk gone"));
    assert_eq!(note(&failed.root).await.unwrap_err().to_string(), "disk gone");
    assert_eq!(closed_reason(&failed.harness).await, ("failed".into(), Some("disk gone".into())));
    assert_eq!(messages(&failed.reports), ["disk gone"]);
}

#[tokio::test]
async fn rejects_the_harness_s_own_calls_with_session_failed_after_a_failure() {
    let storage = Arc::new(ControlledStorage::default());
    let Started { harness, root, .. } = started(storage.clone()).await;
    storage.fail_next_commit(DurableError::storage("disk gone"));
    assert_eq!(note(&root).await.unwrap_err().to_string(), "disk gone");
    assert!(failed_by(&harness.resume().unwrap_err(), "disk gone"));
    assert!(failed_by(&harness.root(&context(), CreateOptions::default()).await.map(|_| ()).unwrap_err(), "disk gone"));
    assert!(failed_by(&harness.conversation(root.id, &context()).await.map(|_| ()).unwrap_err(), "disk gone"));
    let ownerless = CreateOptions { ownership: ConversationOwnership::Ownerless, ..CreateOptions::default() };
    assert!(failed_by(&harness.create_conversation(ownerless, &context()).await.map(|_| ()).unwrap_err(), "disk gone"));
    harness.close(&context()).await.ok();
}

#[tokio::test]
async fn ends_what_a_task_holds_with_session_failed_not_as_cancelled_by_its_signal() {
    let ends = Arc::new(Mutex::new(Vec::<String>::new()));
    let watching = Deferred::default();
    let (sink, signal) = (ends.clone(), watching.clone());
    let watcher = define_task(
        TaskDefinition::new("test.watcher", 1, |_| Ok(Json::from(json!({ "phase": "work" }))))
            .phase("work", move |_, runtime, context| {
                let (sink, watching) = (sink.clone(), signal.clone());
                async move {
                    let doc = runtime.watch_doc(&LIVE_DOC, runtime.conversation_id(), &context).await?.unwrap();
                    let handle = runtime.conversation(runtime.conversation_id(), &context).await?.unwrap();
                    let idle_context = context.clone();
                    let idle = tokio::spawn(async move {
                        handle
                            .wait_for_idle(&idle_context)
                            .await
                            .map_or_else(|error| error.name().to_owned(), |()| "resolved".into())
                    });
                    watching.resolve();
                    let end = doc.closed().await;
                    sink.lock().unwrap().push(format!("{end:?}"));
                    let idle = idle.await.unwrap();
                    sink.lock().unwrap().push(idle);
                    Ok(())
                }
            })
            .abort(|_, _, _| async { Ok(()) }),
    );
    let storage = Arc::new(ControlledStorage::default());
    let opened = open_tasks(storage.clone(), vec![watcher.clone()]).await;
    let root = opened.harness.root(&context(), CreateOptions::default()).await.unwrap();
    let id = root.id;
    root.commit(
        move |tx: Tx| async move { tx.create_task(watcher, Json::Null, TaskOptions::conversation(id)).await },
        &context(),
    )
    .await
    .unwrap();
    opened.harness.resume().unwrap();
    watching.wait().await;
    storage.fail_next_commit(DurableError::storage("disk gone"));
    assert_eq!(note(&root).await.unwrap_err().to_string(), "disk gone");
    opened.harness.closed().await;
    support::task_support::eventually(|| async { ends.lock().unwrap().len() == 2 }).await;
    let ends = ends.lock().unwrap().clone();
    assert!(ends[0].starts_with("Failed("), "{ends:?}");
    assert_eq!(ends[1], "SessionFailed");
    opened.harness.close(&context()).await.ok();
}

#[tokio::test]
async fn fails_it_from_a_failed_id_mint() {
    let storage = Arc::new(ControlledStorage::default());
    let Started { harness, root, .. } = started(storage.clone()).await;
    fail_once(&storage, Arc::new(Mutex::new(vec!["mint_id"])), None);
    assert_eq!(note(&root).await.unwrap_err().to_string(), "disk gone");
    assert_eq!(closed_reason(&harness).await, ("failed".into(), Some("disk gone".into())));
    harness.close(&context()).await.ok();
}

#[tokio::test]
async fn does_not_commit_what_a_callback_did_after_it_caught_a_failed_read() {
    let storage = Arc::new(ControlledStorage::default());
    let Started { harness, root, .. } = started(storage.clone()).await;
    let written = storage.commit_count();
    fail_once(&storage, Arc::new(Mutex::new(vec!["task"])), None);
    let id = root.id;
    let commit = move |tx: Tx| async move {
        let _ = tx.task(TaskId(1)).await;
        tx.append_entry(id, EntryDraft::new("note")).await.map(|_| ())
    };
    assert!(failed_by(&root.commit(commit, &context()).await.unwrap_err(), "disk gone"));
    assert_eq!(storage.commit_count(), written);
    harness.close(&context()).await.ok();
}

#[tokio::test]
async fn fails_it_for_a_storage_request_error_from_a_commit_whose_effect_is_unknown() {
    let storage = Arc::new(ControlledStorage::default());
    let Started { harness, root, .. } = started(storage.clone()).await;
    storage.fail_next_commit(DurableError::storage_request("bad batch"));
    assert_eq!(note(&root).await.unwrap_err().to_string(), "bad batch");
    assert_eq!(closed_reason(&harness).await, ("failed".into(), Some("bad batch".into())));
    harness.close(&context()).await.ok();
}

#[tokio::test]
async fn fails_it_for_a_throw_in_its_own_commit_listeners_which_would_leave_memory_behind_storage() {
    let Started { harness, root, reports, .. } = started(Arc::new(ControlledStorage::default())).await;
    let _unsubscribe = harness.observe_commits(|_, _| Err(DurableError::error("listener bug"))).unwrap();
    // The commit itself is durable and resolves; the Session fails behind it.
    note(&root).await.unwrap();
    assert_eq!(closed_reason(&harness).await, ("failed".into(), Some("listener bug".into())));
    assert_eq!(messages(&reports), ["listener bug"]);
    harness.close(&context()).await.ok();
}

#[tokio::test]
async fn fails_it_for_a_storage_error_during_a_host_close_before_the_backend_closes() {
    let storage = Arc::new(ControlledStorage::default());
    let Started { harness, root, reports, .. } = started(storage.clone()).await;
    let held = storage.hold_commits();
    storage.fail_next_commit(DurableError::storage("disk gone"));
    let committer = root.clone();
    let committing = tokio::spawn(async move { note(&committer).await });
    held.entered().await;
    let closer = harness.clone();
    let closing = tokio::spawn(async move { closer.close(&context()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    held.release();
    assert_eq!(committing.await.unwrap().unwrap_err().to_string(), "disk gone");
    let _ = closing.await.unwrap();
    assert_eq!(closed_reason(&harness).await, ("failed".into(), Some("disk gone".into())));
    assert_eq!(messages(&reports), ["disk gone"]);
}

#[tokio::test]
async fn ends_an_abort_that_joins_a_run_ignoring_its_signal_and_a_reentrant_report_finds_the_session_failed() {
    let (release, entered) = (Deferred::default(), Deferred::default());
    let (gate, signal) = (release.clone(), entered.clone());
    let stubborn = define_task(
        TaskDefinition::new("test.stubborn", 1, |_| Ok(Json::from(json!({ "phase": "work" }))))
            .phase("work", move |_, _, _| {
                let (gate, entered) = (gate.clone(), signal.clone());
                async move {
                    entered.resolve();
                    gate.wait().await;
                    Ok(())
                }
            })
            .abort(|_, _, _| async { Ok(()) }),
    );
    let storage = Arc::new(ControlledStorage::default());
    let registry = Arc::new(create_registry());
    registry.install(Extension::new("tasks").task(stubborn.clone())).unwrap();
    let cell: Arc<OnceLock<Harness>> = Arc::default();
    let reentered = Arc::new(Mutex::new(None::<tokio::task::JoinHandle<DurableResult<()>>>));
    let (harness_cell, slot) = (cell.clone(), reentered.clone());
    let options = HarnessOptions::new(Arc::new(create_models()), registry).with_on_report(move |_, _| {
        let mut slot = slot.lock().unwrap();
        if slot.is_none()
            && let Some(harness) = harness_cell.get().cloned()
        {
            *slot = Some(tokio::spawn(async move { harness.wait_for_idle(&context()).await }));
        }
    });
    let harness = Harness::open(storage.clone(), options, &context()).await.unwrap();
    let _ = cell.set(harness.clone());
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    let id = root.id;
    let id = root
        .commit(
            move |tx: Tx| async move { tx.create_task(stubborn, Json::Null, TaskOptions::conversation(id)).await },
            &context(),
        )
        .await
        .unwrap();
    harness.resume().unwrap();
    entered.wait().await;
    let held = storage.hold_commits();
    let aborter = harness.clone();
    let aborting = tokio::spawn(async move { aborter.abort_task(id, &context()).await.map(|_| ()) });
    held.entered().await;
    held.release();
    // The abort mark commits and the abort joins the run, which ignores its signal; then the Session fails.
    tokio::time::sleep(Duration::from_millis(5)).await;
    storage.fail_next_commit(DurableError::storage("disk gone"));
    assert_eq!(note(&root).await.unwrap_err().to_string(), "disk gone");
    assert!(failed_by(&aborting.await.unwrap().unwrap_err(), "disk gone"));
    let reentered = reentered.lock().unwrap().take().unwrap();
    assert!(failed_by(&reentered.await.unwrap().unwrap_err(), "disk gone"));
    release.resolve();
    harness.close(&context()).await.ok();
}
