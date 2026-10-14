//! The upstream "task close" cases.

use std::sync::LazyLock;

use pi_durable::documents::*;
use pi_durable::harness::registry::{RegistryListener, RegistryUnsubscribe};
use pi_durable::session::WatchEnd;
use serde::{Deserialize, Serialize};

use super::abort::mark_durably;
use super::phases::checkpoint;
use super::support::task_support::open_tasks;
use super::support::{ControlledStorage, Gate};
use super::*;

type Slot<T> = Arc<Mutex<Option<T>>>;

async fn open_controlled(tasks: Vec<Task>) -> (Arc<ControlledStorage>, Harness, Conversation) {
    let storage = Arc::new(ControlledStorage::default());
    let harness = open_tasks(storage.clone(), tasks).await.harness;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    (storage, harness, root)
}

/// The task writes of `id` across every commit, as their statuses.
fn task_statuses(storage: &ControlledStorage, id: TaskId) -> Vec<TaskStatus> {
    let writes = storage.commits().into_iter().flatten();
    writes
        .filter_map(|write| match write {
            StorageWrite::Task { value } if value.id == id => Some(value.state.status()),
            _ => None,
        })
        .collect()
}

fn last_task_writes(storage: &ControlledStorage) -> Vec<TaskRecord> {
    let writes = storage.commits().pop().unwrap().into_iter();
    writes.filter_map(|write| if let StorageWrite::Task { value } = write { Some(value) } else { None }).collect()
}

/// Hold the line with a blocker commit of `root` without waiting for it.
fn hold_line(storage: &ControlledStorage, root: &Conversation) -> Gate {
    let held = storage.hold_commits();
    let (root, id) = (root.clone(), root.id);
    tokio::spawn(async move {
        let blocker = move |tx: Tx| async move { tx.append_entry(id, EntryDraft::new("blocker")).await };
        let _ = root.commit(blocker, &context()).await;
    });
    held
}

#[tokio::test]
async fn stops_without_outcomes_and_starts_no_fresh_phase_or_abort_invocation_while_closing() {
    let (reached, proceed) = (Deferred::default(), Deferred::default());
    let phases = Arc::new(Mutex::new(Vec::new()));
    let abort_ran = Arc::new(Mutex::new(false));
    let (signal_reached, held, one_sink, two_sink, ran) =
        (reached.clone(), proceed.clone(), phases.clone(), phases.clone(), abort_ran.clone());
    let two = define_task(
        TaskDefinition::new("test.two", 1, |_| Ok(Json::from(json!({ "phase": "one" }))))
            .phase("one", move |_, runtime, context| {
                one_sink.lock().unwrap().push("one");
                let (reached, proceed) = (signal_reached.clone(), held.clone());
                async move {
                    runtime.commit(|_, _| async { Ok(checkpoint("two")) }, &context).await?;
                    reached.resolve();
                    // Ignores signals and returns normally once released; the closing rule wins over the next phase.
                    proceed.wait().await;
                    Ok(())
                }
            })
            .phase("two", move |_, _, _| {
                two_sink.lock().unwrap().push("two");
                async { Ok(()) }
            })
            .abort(move |_, _, _| {
                *ran.lock().unwrap() = true;
                async { Ok(()) }
            }),
    );
    let (storage, harness, root) = open_controlled(vec![two.clone()]).await;
    let id = start(&root, &two, false).await;
    harness.resume().unwrap();
    reached.wait().await;
    let aborting = mark_durably(&harness, id, context()).await;
    let record = harness.get_task(id, &context()).await.unwrap().unwrap();
    let commits = storage.commit_count();
    let closing = harness.close(&context());
    proceed.resolve();
    closing.await.unwrap();
    assert_eq!(aborting.await.unwrap().unwrap(), AbortTaskResult::Marked);
    assert_eq!(storage.commit_count(), commits);
    assert_eq!(*phases.lock().unwrap(), ["one"]);
    assert!(!*abort_ran.lock().unwrap());
    assert!(record.abort_requested);
    assert_eq!(serde_json::to_value(&record.state).unwrap()["checkpoint"], json!({ "phase": "two" }));
    assert_eq!(record.state.status(), TaskStatus::Running);
    let error = root.commit(|_: Tx| async { Ok(()) }, &context()).await.unwrap_err();
    assert!(error.to_string().contains("closed"), "{error}");
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Notes {
    text: String,
}

static CLOSE_NOTES: LazyLock<DocToken<Notes, SessionScope>> =
    LazyLock::new(|| define_doc(DocSpec::new("test.close-notes", 1, SessionScope, Notes::default)).unwrap());

#[tokio::test]
async fn seals_admission_before_signalling_handlers_and_stops_watches_before_joining_them() {
    let reached = Deferred::default();
    let from_listener: Slot<tokio::task::JoinHandle<DurableResult<()>>> = Arc::default();
    let watch_end: Slot<WatchEnd> = Arc::default();
    let root_slot: Slot<Conversation> = Arc::default();
    let (signal_reached, listener_slot, end_slot, roots) =
        (reached.clone(), from_listener.clone(), watch_end.clone(), root_slot.clone());
    let stubborn = one_step("test.stubborn", move |_, runtime, _| {
        let (reached, listener_slot, end_slot) = (signal_reached.clone(), listener_slot.clone(), end_slot.clone());
        let root = roots.lock().unwrap().clone().unwrap();
        async move {
            // Uses a context close does not cancel, then waits for the watch to close.
            let watch = runtime.watch_doc(&CLOSE_NOTES, (), &context()).await?.unwrap();
            runtime.signal().add_listener(move |_| {
                // Rust futures are lazy, so the commit reaches the Session just after the listener returns.
                let committing = async move { root.commit(|_: Tx| async { Ok(()) }, &context()).await };
                *listener_slot.lock().unwrap() = Some(tokio::spawn(committing));
            });
            reached.resolve();
            *end_slot.lock().unwrap() = Some(watch.closed().await);
            Ok(())
        }
    });
    let (harness, root, _) = open_root(vec![stubborn.clone()]).await;
    *root_slot.lock().unwrap() = Some(root.clone());
    let write = |tx: Tx| async move { tx.doc(&CLOSE_NOTES, ()).await?.edit(|notes| notes.text = "x".into()) };
    root.commit(write, &context()).await.unwrap();
    start(&root, &stubborn, false).await;
    harness.resume().unwrap();
    reached.wait().await;
    harness.close(&context()).await.unwrap();
    let from_listener = from_listener.lock().unwrap().take().unwrap();
    let error = from_listener.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("closed"), "{error}");
    assert!(matches!(watch_end.lock().unwrap().clone(), Some(WatchEnd::SessionClosed)));
}

#[tokio::test]
async fn writes_no_fault_when_close_seals_while_the_step_after_a_failed_phase_is_queued() {
    let held: Slot<Gate> = Arc::default();
    let storage_slot: Slot<(Arc<ControlledStorage>, Conversation)> = Arc::default();
    let (held_slot, slots) = (held.clone(), storage_slot.clone());
    let throws = one_step("test.close-fault", move |_, _, _| {
        let (storage, root) = slots.lock().unwrap().clone().unwrap();
        let gate = hold_line(&storage, &root);
        *held_slot.lock().unwrap() = Some(gate.clone());
        // Upstream's commit enters the line at once; here the blocker must hold it before the phase fails.
        async move {
            gate.entered().await;
            Err(DurableError::error("would fault"))
        }
    });
    let (storage, harness, root) = open_controlled(vec![throws.clone()]).await;
    *storage_slot.lock().unwrap() = Some((storage.clone(), root.clone()));
    let id = start(&root, &throws, false).await;
    harness.resume().unwrap();
    let ready = held.clone();
    eventually(move || ready.lock().unwrap().is_some()).await;
    let gate = held.lock().unwrap().clone().unwrap();
    gate.entered().await;
    tokio::time::sleep(Duration::from_millis(5)).await;
    let closing = harness.close(&context());
    gate.release();
    closing.await.unwrap();
    assert_eq!(task_statuses(&storage, id), [TaskStatus::Pending, TaskStatus::Running]);
}

#[tokio::test]
async fn starts_no_abort_handler_whose_reservation_settles_while_closing() {
    let ran = Arc::new(Mutex::new(false));
    let sink = ran.clone();
    let marked = define_task(
        TaskDefinition::new("test.close-abort-reservation", 1, |_| Ok(Json::from(json!({ "phase": "run" }))))
            .phase("run", |_, _, _| async { Ok(()) })
            .abort(move |_, _, _| {
                *sink.lock().unwrap() = true;
                async { Ok(()) }
            }),
    );
    let (storage, harness, root) = open_controlled(vec![marked.clone()]).await;
    let id = start(&root, &marked, false).await;
    assert_eq!(harness.abort_task(id, &context()).await.unwrap(), AbortTaskResult::Marked);
    let held = storage.hold_commits();
    harness.resume().unwrap();
    held.entered().await;
    let closing = harness.close(&context());
    held.release();
    closing.await.unwrap();
    assert!(!*ran.lock().unwrap());
    let writes = last_task_writes(&storage);
    assert_eq!(writes.len(), 1);
    assert_eq!((writes[0].id, writes[0].abort_requested, writes[0].state.status()), (id, true, TaskStatus::Running));
}

#[tokio::test]
async fn rejects_a_runtime_commit_that_was_queued_on_the_line_when_close_sealed_it() {
    let held: Slot<Gate> = Arc::default();
    let queued: Slot<tokio::task::JoinHandle<DurableResult<()>>> = Arc::default();
    let storage_slot: Slot<(Arc<ControlledStorage>, Conversation)> = Arc::default();
    let proceed = Deferred::default();
    let (held_slot, queued_slot, slots, release) =
        (held.clone(), queued.clone(), storage_slot.clone(), proceed.clone());
    let task = one_step("test.close-queued-commit", move |_, runtime, _| {
        let (storage, root) = slots.lock().unwrap().clone().unwrap();
        let (held_slot, queued_slot, release) = (held_slot.clone(), queued_slot.clone(), release.clone());
        async move {
            let gate = hold_line(&storage, &root);
            gate.entered().await;
            *held_slot.lock().unwrap() = Some(gate);
            let commit = async move { runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &context()).await };
            *queued_slot.lock().unwrap() = Some(tokio::spawn(commit));
            // Ignores the close signal, so the invocation is still alive when the queued commit reaches the line.
            release.wait().await;
            Ok(())
        }
    });
    let (storage, harness, root) = open_controlled(vec![task.clone()]).await;
    *storage_slot.lock().unwrap() = Some((storage.clone(), root.clone()));
    let id = start(&root, &task, false).await;
    harness.resume().unwrap();
    let ready = held.clone();
    eventually(move || ready.lock().unwrap().is_some()).await;
    tokio::time::sleep(Duration::from_millis(5)).await;
    let closer = harness.clone();
    let closing = tokio::spawn(async move { closer.close(&context()).await });
    tokio::task::yield_now().await;
    held.lock().unwrap().clone().unwrap().release();
    let queued = queued.lock().unwrap().take().unwrap();
    let error = queued.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("Harness is closed"), "{error}");
    proceed.resolve();
    closing.await.unwrap().unwrap();
    assert_eq!(task_statuses(&storage, id).len(), 2);
}

/// A registry reader whose next snapshot, once armed, starts closing the Harness.
struct ClosingReader {
    registry: Arc<Registry>,
    harness: Slot<Harness>,
    armed: Arc<Mutex<bool>>,
    closing: Slot<tokio::task::JoinHandle<DurableResult<()>>>,
}

impl RegistryReader for ClosingReader {
    fn snapshot(&self) -> RegistrySnapshot {
        if std::mem::take(&mut *self.armed.lock().unwrap()) {
            let harness = self.harness.lock().unwrap().clone().unwrap();
            *self.closing.lock().unwrap() = Some(tokio::spawn(harness.close(&context())));
        }
        self.registry.snapshot()
    }

    fn subscribe(&self, listener: RegistryListener) -> RegistryUnsubscribe {
        self.registry.subscribe(listener)
    }
}

#[tokio::test]
async fn starts_no_next_phase_when_close_seals_during_a_step_that_decided_to_continue() {
    let phases = Arc::new(Mutex::new(Vec::new()));
    let armed = Arc::new(Mutex::new(false));
    let (one_sink, two_sink, arm) = (phases.clone(), phases.clone(), armed.clone());
    // The step refreshes the snapshot after progress, inside its line callback and after its closing check.
    let two = define_task(
        TaskDefinition::new("test.close-in-step", 1, |_| Ok(Json::from(json!({ "phase": "one" }))))
            .phase("one", move |_, runtime, context| {
                one_sink.lock().unwrap().push("one");
                let arm = arm.clone();
                async move {
                    runtime.commit(|_, _| async { Ok(checkpoint("two")) }, &context).await?;
                    *arm.lock().unwrap() = true;
                    Ok(())
                }
            })
            .phase("two", move |_, _, _| {
                two_sink.lock().unwrap().push("two");
                async { Ok(()) }
            })
            .abort(|_, _, _| async { Ok(()) }),
    );
    let registry = Arc::new(create_registry());
    registry.install(Extension::new("tasks").task(two.clone())).unwrap();
    let reader = Arc::new(ClosingReader { registry, harness: Arc::default(), armed, closing: Arc::default() });
    let models = support::harness_support::chat_setup(Default::default()).models;
    let options = HarnessOptions::new(models, reader.clone() as Arc<dyn RegistryReader>);
    let harness = Harness::open(Arc::new(MemoryStorage::new()), options, &context()).await.unwrap();
    *reader.harness.lock().unwrap() = Some(harness.clone());
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    start(&root, &two, false).await;
    harness.resume().unwrap();
    let closing = reader.closing.clone();
    eventually(move || closing.lock().unwrap().is_some()).await;
    let closing = reader.closing.lock().unwrap().take().unwrap();
    closing.await.unwrap().unwrap();
    assert_eq!(*phases.lock().unwrap(), ["one"]);
    reader.harness.lock().unwrap().take();
}

#[tokio::test]
async fn joins_a_reservation_that_settles_while_closing_without_starting_its_handler() {
    let ran = Arc::new(Mutex::new(false));
    let sink = ran.clone();
    let never = one_step("test.close-reservation", move |_, _, _| {
        *sink.lock().unwrap() = true;
        async { Ok(()) }
    });
    let (storage, harness, root) = open_controlled(vec![never.clone()]).await;
    let id = start(&root, &never, false).await;
    let held = storage.hold_commits();
    harness.resume().unwrap();
    held.entered().await;
    let closing = harness.close(&context());
    held.release();
    closing.await.unwrap();
    assert!(!*ran.lock().unwrap());
    let writes = last_task_writes(&storage);
    assert_eq!(writes.len(), 1);
    assert_eq!((writes[0].id, writes[0].state.status()), (id, TaskStatus::Running));
}
