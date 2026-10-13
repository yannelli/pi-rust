//! Port of upstream `test/harness-lifecycle.test.ts`. This file holds the helpers and the open and close cases;
//! `sealed.rs` holds operations after close begins, and `paused.rs` paused scheduling and registry changes.

#[path = "../support/mod.rs"]
mod support;

mod paused;
mod sealed;

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use pi_chord::state::{DeliveryKind, StateListener};
use pi_chord::{AbortController, ChordError, Context, Json, with_abort_signal};
use pi_durable::documents::{DocSpec, DocToken, SessionScope, define_doc};
use pi_durable::harness::generation::{GenerationHooks, generation_task};
use pi_durable::harness::registry::{RegistryListener, RegistryUnsubscribe};
use pi_durable::harness::*;
use pi_durable::models::create_models;
use pi_durable::session::{Tx, WatchEnd};
use pi_durable::storage::MemoryStorage;
use pi_durable::storage::sqlite::open_sqlite_storage;
use pi_durable::tasks::TaskOptions;
use pi_durable::*;
use serde_json::json;
use support::chat::{add_tool, answer, call, no_parameters};
use support::harness_support::{chat_setup, open_chat};
use support::task_support::{completed, eventually, open_tasks};
use support::{ControlledStorage, Deferred, context, flush};

type Run = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

/// A one-phase task running `run` and completing with null.
pub fn one_step(name: &str, run: Option<Run>) -> Task {
    define_task(
        TaskDefinition::new(name, 1, |_| Ok(Json::from(json!({ "phase": "run" }))))
            .phase("run", move |_, runtime, context| {
                let run = run.clone();
                async move {
                    if let Some(run) = run {
                        run().await;
                    }
                    runtime.commit(|_, _| async { Ok(completed(serde_json::Value::Null)) }, &context).await
                }
            })
            .abort(|_, _, _| async { Ok(()) }),
    )
}

pub fn runner<F, Fut>(run: F) -> Option<Run>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    Some(Arc::new(move || Box::pin(run())))
}

pub async fn start(conversation: &Conversation, task: &Task, background: bool) -> TaskId {
    let (task, id) = (task.clone(), conversation.id);
    let create = move |tx: Tx| async move {
        tx.create_task(task, Json::Null, TaskOptions { background, ..TaskOptions::conversation(id) }).await
    };
    conversation.commit(create, &context()).await.unwrap()
}

pub fn with_signal(controller: &AbortController) -> Context {
    with_abort_signal(&controller.signal(), &context())
}

/// Whether a raw Storage read still succeeds, as code of a joined invocation relies on.
pub async fn storage_open(storage: &dyn Storage) -> bool {
    storage.task(TaskId(1), &context()).await.is_ok()
}

/// Whether `future` settled within a few scheduler turns.
pub async fn settled<T>(future: &mut tokio::task::JoinHandle<T>) -> bool {
    tokio::time::timeout(Duration::from_millis(20), future).await.is_ok()
}

pub fn notes_doc(kind: &str) -> DocToken<Json, SessionScope> {
    define_doc(DocSpec::new(kind, 1, SessionScope, || Json::from(json!({ "text": "" })))).unwrap()
}

pub async fn set_text(harness: &Harness, doc: &DocToken<Json, SessionScope>, text: &str) {
    let (doc, text) = (doc.clone(), text.to_owned());
    let commit = move |tx: Tx| async move { tx.doc(&doc, ()).await?.set(&Json::from(json!({ "text": text }))) };
    harness.commit(commit, &context()).await.unwrap();
}

/// Commit a conversation with a `running` task, as a crash leaves it, so open has a reconciliation commit to fail.
async fn seed_running_task(storage: &dyn Storage) {
    let conversation_id = storage.mint_id().await.unwrap();
    let id = storage.mint_id().await.unwrap();
    let task: TaskRecord = serde_json::from_value(json!({
        "id": id, "conversationId": conversation_id, "kind": "test.seeded", "version": 1, "input": null,
        "background": false, "abortRequested": false, "state": { "status": "running", "checkpoint": { "phase": "run" } },
    }))
    .unwrap();
    let conversation: ConversationRecord = serde_json::from_value(json!({ "id": conversation_id })).unwrap();
    let writes = [StorageWrite::Conversation { value: conversation }, StorageWrite::Task { value: task }];
    storage.commit(&writes, &context()).await.unwrap();
}

/// A registry reader that counts live subscriptions.
struct CountingReader {
    registry: Registry,
    count: Arc<AtomicUsize>,
}

impl RegistryReader for CountingReader {
    fn snapshot(&self) -> RegistrySnapshot {
        self.registry.snapshot()
    }

    fn subscribe(&self, listener: RegistryListener) -> RegistryUnsubscribe {
        self.count.fetch_add(1, Ordering::SeqCst);
        let unsubscribe = self.registry.subscribe(listener);
        let count = self.count.clone();
        Box::new(move || {
            count.fetch_sub(1, Ordering::SeqCst);
            unsubscribe();
        })
    }
}

#[tokio::test]
async fn closes_without_a_cancelled_caller_context_and_rethrows_the_original_error_when_open_fails() {
    let storage = Arc::new(ControlledStorage::default());
    seed_running_task(storage.as_ref()).await;
    let count = Arc::new(AtomicUsize::new(0));
    let reader = Arc::new(CountingReader { registry: create_registry(), count: count.clone() });
    let held = storage.hold_commits();
    storage.fail_next_commit(DurableError::error("disk full"));
    let controller = AbortController::new();
    let options = HarnessOptions::new(Arc::new(create_models()), reader);
    let (opening_storage, opening_context) = (storage.clone(), with_signal(&controller));
    let opening =
        tokio::spawn(async move { Harness::open(opening_storage, options, &opening_context).await.map(|_| ()) });
    held.entered().await;
    controller.abort(Some(ChordError::error("caller gave up")));
    held.release();
    let error = opening.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("disk full"), "{error}");
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert!(!storage_open(storage.as_ref()).await);
}

#[tokio::test]
async fn rethrows_the_open_error_and_reports_it_once_though_closing_then_fails_too() {
    let storage = Arc::new(ControlledStorage::default());
    *storage.close_failure.lock().unwrap() = Some(DurableError::error("close failed"));
    seed_running_task(storage.as_ref()).await;
    storage.fail_next_commit(DurableError::error("disk full"));
    let reports = Arc::new(Mutex::new(Vec::new()));
    let sink = reports.clone();
    let options = HarnessOptions::new(Arc::new(create_models()), Arc::new(create_registry()))
        .with_on_report(move |error| sink.lock().unwrap().push(error.to_string()));
    let error = Harness::open(storage, options, &context()).await.map(|_| ()).unwrap_err();
    assert!(error.to_string().contains("disk full"), "{error}");
    // The storage failure that failed open is the cause; the close that fails after it adds no report.
    let reports = reports.lock().unwrap().clone();
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert!(reports[0].contains("disk full"));
}

/// A run that signals `reached`, waits for `gate`, then records whether Storage still reads.
fn stubborn(
    storage: Arc<dyn Storage>,
    reached: &Deferred,
    gate: &Deferred,
    read: &Arc<Mutex<Option<bool>>>,
) -> Option<Run> {
    let (reached, gate, read) = (reached.clone(), gate.clone(), read.clone());
    runner(move || {
        let (reached, gate, read, storage) = (reached.clone(), gate.clone(), read.clone(), storage.clone());
        async move {
            reached.resolve();
            gate.wait().await;
            *read.lock().unwrap() = Some(storage_open(storage.as_ref()).await);
        }
    })
}

fn closing(harness: &Harness) -> tokio::task::JoinHandle<DurableResult<()>> {
    let harness = harness.clone();
    tokio::spawn(async move { harness.close(&context()).await })
}

#[tokio::test]
async fn joins_a_task_handler_that_ignores_its_signal_before_closing_storage() {
    let storage = Arc::new(MemoryStorage::new());
    let (reached, gate, read) = (Deferred::default(), Deferred::default(), Arc::default());
    let task = one_step("test.close-stubborn", stubborn(storage.clone(), &reached, &gate, &read));
    let harness = open_tasks(storage.clone(), vec![task.clone()]).await.harness;
    start(&harness.root(&context(), CreateOptions::default()).await.unwrap(), &task, false).await;
    harness.resume().unwrap();
    reached.wait().await;
    let mut close = closing(&harness);
    assert!(!settled(&mut close).await);
    assert!(storage_open(storage.as_ref()).await);
    gate.resolve();
    close.await.unwrap().unwrap();
    assert_eq!(*read.lock().unwrap(), Some(true));
    assert!(!storage_open(storage.as_ref()).await);
}

#[tokio::test]
async fn joins_a_tool_execute_and_a_hook_that_ignore_their_signal_before_closing_storage() {
    for place in ["tool", "hook"] {
        let setup = chat_setup(Default::default());
        let storage = Arc::new(MemoryStorage::new());
        let (reached, gate, read) = (Deferred::default(), Deferred::default(), Arc::default());
        let wait = stubborn(storage.clone(), &reached, &gate, &read).unwrap();
        let in_tool = wait.clone();
        add_tool(
            &setup,
            define_tool("wait", "wait", no_parameters(), move |_, _, _| {
                let wait = in_tool.clone();
                async move {
                    if place == "tool" {
                        wait().await;
                    }
                    Ok(ToolExecutionResult { output: Some(Vec::new()), ..Default::default() })
                }
            }),
        );
        let hooks = GenerationHooks {
            before_request: Some(Arc::new(move |_, _, _| {
                let wait = wait.clone();
                Box::pin(async move {
                    if place == "hook" {
                        wait().await;
                    }
                    Ok(None)
                })
            })),
            ..GenerationHooks::default()
        };
        setup.registry.install(Extension::new("hooks").hook(hook(&generation_task(), hooks))).unwrap();
        setup.faux.set_responses([call("wait", "c1"), answer("done")]);
        let (harness, root) = open_chat(storage.clone(), &setup).await;
        root.submit(SubmissionDraft::input("go"), &context()).await.unwrap();
        reached.wait().await;
        let mut close = closing(&harness);
        assert!(!settled(&mut close).await);
        assert!(storage_open(storage.as_ref()).await);
        gate.resolve();
        close.await.unwrap().unwrap();
        assert_eq!(*read.lock().unwrap(), Some(true));
        assert!(!storage_open(storage.as_ref()).await);
    }
}

#[tokio::test]
async fn lets_a_new_harness_open_the_same_storage_once_close_resolved_no_old_invocation_code_runs() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite").to_string_lossy().into_owned();
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let (gate, generation) = (Deferred::default(), Arc::new(AtomicUsize::new(1)));
    let (sink, wait, current) = (log.clone(), gate.clone(), generation.clone());
    let task = one_step(
        "test.generations",
        runner(move || {
            let (sink, gate, mine) = (sink.clone(), wait.clone(), current.load(Ordering::SeqCst));
            async move {
                sink.lock().unwrap().push(format!("start {mine}"));
                if mine == 1 {
                    gate.wait().await;
                }
                sink.lock().unwrap().push(format!("end {mine}"));
            }
        }),
    );
    let first = open_tasks(Arc::new(open_sqlite_storage(&path).await.unwrap()), vec![task.clone()]).await.harness;
    let id = start(&first.root(&context(), CreateOptions::default()).await.unwrap(), &task, false).await;
    first.resume().unwrap();
    eventually(|| async { log.lock().unwrap().len() == 1 }).await;
    let (closing_log, closing_harness) = (log.clone(), first.clone());
    let mut close = tokio::spawn(async move {
        closing_harness.close(&context()).await.unwrap();
        closing_log.lock().unwrap().push("closed".into());
    });
    assert!(!settled(&mut close).await);
    gate.resolve();
    close.await.unwrap();
    assert_eq!(*log.lock().unwrap(), ["start 1", "end 1", "closed"]);
    generation.store(2, Ordering::SeqCst);
    let second = open_tasks(Arc::new(open_sqlite_storage(&path).await.unwrap()), vec![task]).await.harness;
    second.resume().unwrap();
    second.wait_for_task(id, &context()).await.unwrap();
    assert_eq!(*log.lock().unwrap(), ["start 1", "end 1", "closed", "start 2", "end 2"]);
    second.close(&context()).await.unwrap();
}

#[tokio::test]
async fn keeps_shutting_down_after_a_cancelled_close_and_a_second_close_awaits_the_same_shutdown() {
    let storage = Arc::new(MemoryStorage::new());
    let (reached, gate) = (Deferred::default(), Deferred::default());
    let (signal, wait) = (reached.clone(), gate.clone());
    let task = one_step(
        "test.close-cancelled",
        runner(move || {
            let (reached, gate) = (signal.clone(), wait.clone());
            async move {
                reached.resolve();
                gate.wait().await;
            }
        }),
    );
    let harness = open_tasks(storage.clone(), vec![task.clone()]).await.harness;
    start(&harness.root(&context(), CreateOptions::default()).await.unwrap(), &task, false).await;
    harness.resume().unwrap();
    reached.wait().await;
    let controller = AbortController::new();
    let (cancelled_harness, cancelled_context) = (harness.clone(), with_signal(&controller));
    let cancelled = tokio::spawn(async move { cancelled_harness.close(&cancelled_context).await });
    flush().await;
    controller.abort(Some(ChordError::error("stop waiting")));
    let error = cancelled.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("stop waiting"), "{error}");
    // Admission stays sealed and the invocation still holds Storage open.
    let error = harness.commit(|_| async { Ok(()) }, &context()).await.unwrap_err();
    assert!(error.to_string().contains("closed"), "{error}");
    assert!(storage_open(storage.as_ref()).await);
    let mut second = closing(&harness);
    assert!(!settled(&mut second).await);
    gate.resolve();
    second.await.unwrap().unwrap();
    assert!(!storage_open(storage.as_ref()).await);
}

#[tokio::test]
async fn settles_durably_a_commit_whose_committer_was_cancelled_while_it_was_in_storage() {
    let storage = Arc::new(ControlledStorage::default());
    let harness = open_tasks(storage.clone(), Vec::new()).await.harness;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    let held = storage.hold_commits();
    let controller = AbortController::new();
    let (committing_root, committing_context, id) = (root.clone(), with_signal(&controller), root.id);
    let committing = tokio::spawn(async move {
        let append = move |tx: Tx| async move { Ok(tx.append_entry(id, EntryDraft::new("note")).await?.id) };
        committing_root.commit(append, &committing_context).await
    });
    held.entered().await;
    controller.abort(Some(ChordError::error("committer gave up")));
    held.release();
    let entry = committing.await.unwrap().unwrap();
    let page = root.entries(EntryRange::default(), 10, None, &context()).await.unwrap();
    assert_eq!(page.items.iter().map(|entry| entry.id).collect::<Vec<_>>(), [entry]);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn publishes_no_frame_to_states_and_watches_from_a_commit_that_settles_during_close() {
    let notes = notes_doc("test.close-frames");
    let storage = Arc::new(ControlledStorage::default());
    let harness = open_tasks(storage.clone(), Vec::new()).await.harness;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    set_text(&harness, &notes, "before").await;
    let doc_state = harness.document_state(&notes, (), &context()).await.unwrap().unwrap();
    let doc_watch = harness.watch_doc(&notes, (), &context()).await.unwrap().unwrap();
    let view_state = root.view_state(&context()).await.unwrap();
    let view_watch = root.watch(&context()).await.unwrap();
    let graph_state = harness.task_graph(&context()).await.unwrap();
    let graph_watch = harness.watch_task_graph(&context()).await.unwrap();
    let frames = Arc::new(Mutex::new(Vec::<&'static str>::new()));
    let updates = |name: &'static str| {
        let sink = frames.clone();
        StateListener::sync(move |_, _, delivery| {
            if delivery.kind == DeliveryKind::Update {
                sink.lock().unwrap().push(name);
            }
            Ok(())
        })
    };
    let _graph = graph_state.subscribe(updates("graphState"));
    let _doc = doc_state.subscribe(updates("docState"));
    let _view = view_state.subscribe(updates("viewState"));
    for (watch, name) in [(&graph_watch, "graphWatch"), (&doc_watch, "docWatch"), (&view_watch, "viewWatch")] {
        let sink = frames.clone();
        watch
            .start(move |_, _, _| {
                sink.lock().unwrap().push(name);
                async { Ok(()) }
            })
            .unwrap();
    }
    let (doc_value, view_value) = (doc_state.value(), view_state.value());

    let held = storage.hold_commits();
    let (committing_harness, doc, id) = (harness.clone(), notes.clone(), root.id);
    let committing = tokio::spawn(async move {
        let commit = move |tx: Tx| async move {
            tx.doc(&doc, ()).await?.set(&Json::from(json!({ "text": "during close" })))?;
            tx.append_entry(id, EntryDraft::new("note")).await?;
            tx.create_task(one_step("test.close-graph", None), Json::Null, TaskOptions::conversation(id)).await?;
            Ok(())
        };
        committing_harness.commit(commit, &context()).await
    });
    held.entered().await;
    let close = closing(&harness);
    flush().await;
    held.release();
    committing.await.unwrap().unwrap();
    close.await.unwrap().unwrap();
    flush().await;
    assert!(frames.lock().unwrap().is_empty(), "{:?}", frames.lock().unwrap());
    assert!(doc_state.value().same_ref(&doc_value));
    assert!(view_state.value().same_ref(&view_value));
    assert_eq!(serde_json::Value::from(&graph_state.value()), json!({ "tasks": {} }));
    for watch in [&graph_watch, &doc_watch, &view_watch] {
        assert!(matches!(watch.closed().await, WatchEnd::SessionClosed));
    }
    // The commit itself settled.
    assert!(storage.last_commit().iter().any(|write| matches!(write, StorageWrite::Entry { .. })));
}
