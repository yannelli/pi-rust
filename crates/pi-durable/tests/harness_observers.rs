//! Port of upstream `test/harness-observers.test.ts`: the Harness's synchronous observers (`observe_conversation()`,
//! `observe_task_graph()`, `observe_doc()` of a task document) and task document watches, which end with their task.

mod support;

use std::sync::{Arc, LazyLock, Mutex};

use pi_chord::{Context, Json, WatchChange};
use pi_durable::documents::*;
use pi_durable::harness::*;
use pi_durable::session::{Observer, Tx, WatchEnd};
use pi_durable::tasks::TaskOptions;
use pi_durable::*;
use serde_json::{Value, json};
use support::task_support::{OpenedTasks, completed, open_tasks};
use support::{ControlledStorage, Deferred, context, flush};

static PROGRESS: LazyLock<DocToken<Json, TaskScope>> = LazyLock::new(|| {
    let initial = || Json::from(json!({ "lines": [] }));
    define_doc(DocSpec::new("test.observers.progress", 1, TaskScope, initial, |_, _, _| Ok(false))).unwrap()
});

#[derive(Clone, Debug, PartialEq)]
enum Call {
    /// A change, and whether it came inside the storage's `adopt`.
    Advance(WatchChange, bool),
    /// The end, and whether it came inside the storage's `adopt`.
    End(Option<String>, bool),
}

/// An observer recording each call and whether it ran inside the storage's `adopt`.
struct Recorder {
    storage: Arc<ControlledStorage>,
    calls: Mutex<Vec<Call>>,
}

impl Observer for Recorder {
    fn advance(&self, change: &WatchChange, _context: &Context) -> DurableResult<()> {
        self.calls.lock().unwrap().push(Call::Advance(change.clone(), self.storage.adopting()));
        Ok(())
    }

    fn end(&self, failure: Option<DurableError>) {
        self.calls.lock().unwrap().push(Call::End(failure.map(|error| error.to_string()), self.storage.adopting()));
    }
}

fn recorder(storage: &Arc<ControlledStorage>) -> Arc<Recorder> {
    Arc::new(Recorder { storage: storage.clone(), calls: Mutex::default() })
}

impl Recorder {
    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
}

async fn note(root: &Conversation) -> DurableResult<EntryRecord> {
    let id = root.id;
    let append = move |tx: Tx| async move { tx.append_entry(id, EntryDraft::new("note")).await };
    root.commit(append, &context()).await
}

/// A task that writes its progress document, waits for `gate`, then completes, which retires the document.
fn writer(gate: Deferred, written: Deferred) -> Task {
    let run = move |task: TaskRecord, runtime: TaskRuntime, ctx: Context| {
        let (gate, written) = (gate.clone(), written.clone());
        async move {
            let id = task.id;
            let start = move |tx: Tx, _| async move {
                tx.doc(&PROGRESS, id).await?.update(|progress| {
                    progress.get_mut("lines").and_then(Json::as_array_mut).unwrap().push(Json::from("started"))
                })?;
                Ok(None)
            };
            runtime.commit(start, &ctx).await?;
            written.resolve();
            gate.wait().await;
            runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &ctx).await
        }
    };
    let definition = TaskDefinition::new("test.observers.writer", 1, |_| Ok(Json::from(json!({ "phase": "run" }))));
    define_task(definition.phase("run", run).abort(|_, _, _| async { Ok(()) }))
}

struct Writer {
    opened: OpenedTasks,
    storage: Arc<ControlledStorage>,
    id: TaskId,
    finish: Deferred,
}

/// A Harness with a writer task started in the root conversation and its progress document written.
async fn open_writer() -> Writer {
    let storage = Arc::new(ControlledStorage::default());
    let (finish, written) = (Deferred::default(), Deferred::default());
    let task = writer(finish.clone(), written.clone());
    let opened = open_tasks(storage.clone(), vec![task.clone()]).await;
    let root = opened.harness.root(&context(), CreateOptions::default()).await.unwrap();
    let conversation = root.id;
    let create =
        move |tx: Tx| async move { tx.create_task(task, Json::Null, TaskOptions::conversation(conversation)).await };
    let id = root.commit(create, &context()).await.unwrap();
    opened.harness.resume().unwrap();
    written.wait().await;
    Writer { opened, storage, id, finish }
}

#[tokio::test]
async fn observe_conversation_and_observe_task_graph_start_at_the_current_value_and_advance_inside_adopt() {
    let storage = Arc::new(ControlledStorage::default());
    let harness = open_tasks(storage.clone(), Vec::new()).await.harness;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    let view = recorder(&storage);
    let view_observation = harness.observe_conversation(root.id, view.clone(), &context(), None).await.unwrap();
    let graph = recorder(&storage);
    let graph_observation = harness.observe_task_graph(graph.clone(), &context()).await.unwrap();
    assert_eq!(view_observation.value["conversation"]["id"], Json::from(root.id.get()));
    assert_eq!(graph_observation.value, Json::from(json!({ "tasks": {} })));
    let watch = root.watch(&context(), None).await.unwrap();
    let watched = Arc::new(Mutex::new(Vec::new()));
    let sink = watched.clone();
    watch
        .start(move |change, _| {
            sink.lock().unwrap().push(change);
            async { Ok(()) }
        })
        .unwrap();
    let entry = note(&root).await.unwrap();
    let calls = view.calls();
    let [Call::Advance(change, true)] = calls.as_slice() else { panic!("{calls:?}") };
    assert!(!change.retired);
    let entries = change.value["entries"].as_array().unwrap();
    assert_eq!(entries.last().unwrap()["id"], Json::from(entry.id.get()));
    flush().await;
    assert_eq!(*watched.lock().unwrap(), std::slice::from_ref(change));
    // No task changed: the graph got nothing.
    assert!(graph.calls().is_empty());
    view_observation.detach();
    note(&root).await.unwrap();
    assert_eq!(view.calls().len(), 1);
    watch.stop().await;
    harness.close(&context()).await.unwrap();
    assert_eq!(graph.calls(), [Call::End(None, false)]);
}

#[tokio::test]
async fn observers_end_with_the_error_that_failed_the_harness() {
    let storage = Arc::new(ControlledStorage::default());
    let harness = open_tasks(storage.clone(), Vec::new()).await.harness;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    let graph = recorder(&storage);
    harness.observe_task_graph(graph.clone(), &context()).await.unwrap();
    storage.fail_next_commit(DurableError::error("disk gone"));
    assert!(note(&root).await.unwrap_err().to_string().contains("disk gone"));
    harness.closed().await;
    assert_eq!(graph.calls(), [Call::End(Some("disk gone".into()), false)]);
}

#[tokio::test]
async fn observers_reject_on_a_closed_harness() {
    let storage = Arc::new(ControlledStorage::default());
    let harness = open_tasks(storage.clone(), Vec::new()).await.harness;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    harness.close(&context()).await.unwrap();
    let view = harness.observe_conversation(root.id, recorder(&storage), &context(), None).await;
    let graph = harness.observe_task_graph(recorder(&storage), &context()).await;
    assert!(view.unwrap_err().to_string().contains("Session is closed"));
    assert!(graph.unwrap_err().to_string().contains("Session is closed"));
}

#[tokio::test]
async fn a_task_document_watch_delivers_the_retirement_of_the_commit_that_ends_its_task_then_ends_ended() {
    let Writer { opened, id, finish, .. } = open_writer().await;
    let harness = opened.harness;
    let watch = harness.watch_doc(&PROGRESS, id, &context()).await.unwrap().unwrap();
    assert_eq!(watch.value(), Json::from(json!({ "lines": ["started"] })));
    let changes = Arc::new(Mutex::new(Vec::new()));
    let sink = changes.clone();
    watch
        .start(move |change, _| {
            sink.lock().unwrap().push(change);
            async { Ok(()) }
        })
        .unwrap();
    finish.resolve();
    harness.wait_for_task(id, &context()).await.unwrap();
    assert!(matches!(watch.closed().await, WatchEnd::Ended));
    let last = Json::from(json!({ "lines": ["started"] }));
    assert_eq!(*changes.lock().unwrap(), [WatchChange { value: last, ops: Vec::new(), retired: true }]);
    assert!(changes.lock().unwrap()[0].value.same_ref(&watch.value()));
    // The task is terminal: its address is absent for good.
    assert!(harness.watch_doc(&PROGRESS, id, &context()).await.unwrap().is_none());
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn a_task_document_observer_gets_the_retirement_then_its_end_inside_the_adopt_of_the_commit_that_ends_the_task() {
    let Writer { opened, storage, id, finish } = open_writer().await;
    let harness = opened.harness;
    let progress = recorder(&storage);
    harness.observe_doc(&PROGRESS, id, progress.clone(), &context()).await.unwrap().unwrap();
    finish.resolve();
    harness.wait_for_task(id, &context()).await.unwrap();
    let last = Json::from(json!({ "lines": ["started"] }));
    let retirement = WatchChange { value: last, ops: Vec::new(), retired: true };
    assert_eq!(progress.calls(), [Call::Advance(retirement, true), Call::End(None, true)]);
    harness.close(&context()).await.unwrap();
    assert_eq!(progress.calls().len(), 2);
}

/// A task that waits for `gate`, then completes.
fn gated(name: &str, gate: Deferred) -> Task {
    let run = move |_: TaskRecord, runtime: TaskRuntime, ctx: Context| {
        let gate = gate.clone();
        async move {
            gate.wait().await;
            runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &ctx).await
        }
    };
    let definition = TaskDefinition::new(name, 1, |_| Ok(Json::from(json!({ "phase": "run" }))));
    define_task(definition.phase("run", run).abort(|_, _, _| async { Ok(()) }))
}

async fn start(root: &Conversation, task: &Task) -> TaskId {
    let (task, conversation) = (task.clone(), root.id);
    let create =
        move |tx: Tx| async move { tx.create_task(task, Json::Null, TaskOptions::conversation(conversation)).await };
    root.commit(create, &context()).await.unwrap()
}

#[tokio::test]
async fn resumes_task_and_idle_waits_only_once_subscribe_commits_listeners_saw_the_terminal_commit() {
    let gate = Deferred::default();
    let task = gated("test.wake-gated", gate.clone());
    let harness = open_tasks(Arc::new(ControlledStorage::default()), vec![task.clone()]).await.harness;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    harness
        .subscribe_commits(move |publication, _| {
            for change in &publication.changes {
                if let CommitChange::Task(record) = change
                    && record.is_terminal()
                {
                    sink.lock().unwrap().push(record.id);
                }
            }
            Ok(())
        })
        .unwrap();
    let id = start(&root, &task).await;
    harness.resume().unwrap();
    let saw = move |seen: &Arc<Mutex<Vec<TaskId>>>| seen.lock().unwrap().contains(&id);
    let (waiter, idler, conversation) = (harness.clone(), harness.clone(), root.clone());
    let (task_seen, idle_seen, root_seen) = (seen.clone(), seen.clone(), seen.clone());
    let task_wait = tokio::spawn(async move { waiter.wait_for_task(id, &context()).await.map(|_| saw(&task_seen)) });
    let idle = tokio::spawn(async move { idler.wait_for_idle(&context()).await.map(|()| saw(&idle_seen)) });
    let root_idle = tokio::spawn(async move { conversation.wait_for_idle(&context()).await.map(|()| saw(&root_seen)) });
    gate.resolve();
    assert!(task_wait.await.unwrap().unwrap());
    assert!(idle.await.unwrap().unwrap());
    assert!(root_idle.await.unwrap().unwrap());
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn signals_a_newly_marked_run_after_host_listeners_saw_the_mark_outside_the_storage_s_adopt() {
    let storage = Arc::new(ControlledStorage::default());
    let order = Arc::new(Mutex::new(Vec::new()));
    let reached = Deferred::default();
    let (sink, entered, adopting) = (order.clone(), reached.clone(), storage.clone());
    let run = move |_: TaskRecord, runtime: TaskRuntime, _: Context| {
        let (sink, entered, adopting) = (sink.clone(), entered.clone(), adopting.clone());
        async move {
            let signal = runtime.signal();
            let aborted = Deferred::default();
            let fired = aborted.clone();
            signal.add_listener(move |_| {
                sink.lock().unwrap().push(format!("abort adopting={}", adopting.adopting()));
                fired.resolve();
            });
            entered.resolve();
            aborted.wait().await;
            Ok(())
        }
    };
    let definition = TaskDefinition::new("test.order-signalled", 1, |_| Ok(Json::from(json!({ "phase": "run" }))));
    let task = define_task(definition.phase("run", run).abort(|_, _, _| async { Ok(()) }));
    let harness = open_tasks(storage.clone(), vec![task.clone()]).await.harness;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    let id = start(&root, &task).await;
    let host = order.clone();
    harness
        .subscribe_commits(move |publication, _| {
            let marked = publication.changes.iter().any(
                |change| matches!(change, CommitChange::Task(record) if record.id == id && record.abort_requested),
            );
            if marked {
                host.lock().unwrap().push("host".to_owned());
            }
            Ok(())
        })
        .unwrap();
    harness.resume().unwrap();
    reached.wait().await;
    assert_eq!(harness.abort_task(id, &context()).await.unwrap(), AbortTaskResult::Marked);
    support::task_support::eventually(|| async { order.lock().unwrap().len() >= 2 }).await;
    // The commit that marks the task comes first; later ones carry the mark too.
    assert_eq!(order.lock().unwrap()[..2], ["host", "abort adopting=false"]);
    harness.close(&context()).await.unwrap();
}
