//! The upstream "task runtime" cases.

use std::sync::LazyLock;

use pi_ai::{Message, UserContent, UserMessage};
use pi_durable::documents::*;
use serde::{Deserialize, Serialize};

use super::support::task_support::{open_tasks, open_tasks_with};
use super::support::{ControlledStorage, Gate};
use super::*;
use pi_durable::session::DocumentWatch;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Notes {
    text: String,
}

fn session_doc(kind: &str) -> DocToken<Notes, SessionScope> {
    define_doc(DocSpec::new(kind, 1, SessionScope, Notes::default, |_, _, _| Ok(false))).unwrap()
}

static NOTES: LazyLock<DocToken<Notes, SessionScope>> = LazyLock::new(|| session_doc("test.task-notes"));
static ABSENT: LazyLock<DocToken<Notes, SessionScope>> = LazyLock::new(|| session_doc("test.task-absent"));
static LATE_NOTES: LazyLock<DocToken<Notes, SessionScope>> = LazyLock::new(|| session_doc("test.late-watch"));
static RUNTIME_NOTES: LazyLock<DocToken<Notes, RewindableConversation>> = LazyLock::new(|| {
    let scope = RewindableConversation(DocumentFork::AsOf);
    define_doc(DocSpec::new("test.runtime-notes", 1, scope, Notes::default, |_, _, _| Ok(false))).unwrap()
});

async fn write_notes(root: &Conversation, token: &'static LazyLock<DocToken<Notes, SessionScope>>, text: &str) {
    let text = text.to_owned();
    let change = move |tx: Tx| async move { tx.doc(&**token, ()).await?.edit(|notes| notes.text = text) };
    root.commit(change, &context()).await.unwrap();
}

fn ended(result: DurableResult<impl Sized>) {
    let error = result.err().expect("the invocation has ended").to_string();
    assert!(error.contains("invocation has ended"), "{error}");
}

type Slot<T> = Arc<Mutex<Option<T>>>;

#[tokio::test]
async fn rejects_runtime_operations_after_the_invocation_ends_and_stops_its_watches() {
    let captured: Slot<(TaskRuntime, pi_chord::Context)> = Arc::default();
    let closed: Slot<DocumentWatch> = Arc::default();
    let absent: Slot<bool> = Arc::default();
    let delivered = Arc::new(Mutex::new(Vec::<String>::new()));
    let (capture, watch_slot, absent_slot, sink) =
        (captured.clone(), closed.clone(), absent.clone(), delivered.clone());
    let watcher = one_step("test.watcher", move |_, runtime, context| {
        let (capture, watch_slot, absent_slot, sink) =
            (capture.clone(), watch_slot.clone(), absent_slot.clone(), sink.clone());
        async move {
            *capture.lock().unwrap() = Some((runtime.clone(), context.clone()));
            *absent_slot.lock().unwrap() = Some(runtime.watch_doc(&ABSENT, (), &context).await?.is_none());
            let watch = runtime.watch_doc(&NOTES, (), &context).await?.unwrap();
            watch.start(move |value, _, _| {
                let text = value.get("text").and_then(Json::as_str).unwrap_or("retired").to_owned();
                sink.lock().unwrap().push(text);
                async { Ok(()) }
            })?;
            *watch_slot.lock().unwrap() = Some(watch);
            runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &context).await
        }
    });
    let (harness, root, _) = open_root(vec![watcher.clone()]).await;
    write_notes(&root, &NOTES, "hello").await;
    let id = start(&root, &watcher, false).await;
    harness.resume().unwrap();
    harness.wait_for_task(id, &context()).await.unwrap();
    assert_eq!(*absent.lock().unwrap(), Some(true));
    // The watch stops when the step after the phase ends the invocation; later commits deliver nothing.
    let watch = closed.lock().unwrap().clone().unwrap();
    assert!(matches!(watch.closed().await, pi_durable::session::WatchEnd::Stopped));
    write_notes(&root, &NOTES, "after").await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(delivered.lock().unwrap().is_empty());
    let (runtime, handler_context) = captured.lock().unwrap().clone().unwrap();
    ended(runtime.commit(|_, _| async { Ok(None) }, &context()).await);
    ended(runtime.memo("x").await);
    ended(runtime.memo_or("x", Json::from(1), &context()).await);
    ended(runtime.sleep_until(0.0, Json::from(json!({ "phase": "run" })), &context()).await);
    ended(runtime.watch_doc(&NOTES, (), &context()).await);
    // The handler's own context is cancelled by now; the ended invocation still wins.
    ended(runtime.agent(&handler_context).await);
    // Rust `now` and `report` are infallible, so the clock and reports stay usable after the invocation ends.
    harness.close(&context()).await.unwrap();
}

fn user(text: &str) -> Message {
    Message::User(UserMessage { content: UserContent::Text(text.into()), timestamp: 1 })
}

#[tokio::test]
async fn reads_committed_documents_and_context_through_the_runtime_and_forwards_the_clock_and_reports() {
    let captured: Slot<TaskRuntime> = Arc::default();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (capture, sink) = (captured.clone(), seen.clone());
    let reader = one_step("test.reader", move |_, runtime, context| {
        let (capture, sink) = (capture.clone(), sink.clone());
        async move {
            *capture.lock().unwrap() = Some(runtime.clone());
            let id = runtime.conversation_id();
            let ids: Vec<EntryId> = runtime.context(id, &context, None).await?.entries.iter().map(|e| e.id).collect();
            let text = |value: Option<Json>| json!(value.unwrap().get("text").cloned().unwrap().to_serde());
            let mut values = vec![text(runtime.snapshot(&RUNTIME_NOTES, id, &context).await?)];
            values.push(text(runtime.snapshot_as_of(&RUNTIME_NOTES, id, ids[0], &context).await?));
            values.push(json!(runtime.context(id, &context, Some(ids[0])).await?.entries.len()));
            values.push(json!(runtime.context(id, &context, Some(ids[1])).await?.messages.len()));
            values.push(json!(runtime.now()));
            runtime.report(&DurableError::error("reported"));
            *sink.lock().unwrap() = values;
            runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &context).await
        }
    });
    let registry = Arc::new(create_registry());
    let opened =
        open_tasks_with(Arc::new(MemoryStorage::new()), vec![reader.clone()], registry, |o| o.with_now(|| 1234.0))
            .await;
    let harness = opened.harness;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    for text in ["one", "two"] {
        let id = root.id;
        let change = move |tx: Tx| async move {
            tx.doc(&RUNTIME_NOTES, id).await?.edit(|notes| notes.text = text.into())?;
            let draft = EntryDraft { model: Some(vec![user(text)]), ..EntryDraft::new("message") };
            tx.append_entry(id, draft).await
        };
        root.commit(change, &context()).await.unwrap();
    }
    let id = start(&root, &reader, false).await;
    harness.resume().unwrap();
    harness.wait_for_task(id, &context()).await.unwrap();
    assert_eq!(*seen.lock().unwrap(), [json!("two"), json!("one"), json!(1), json!(2), json!(1234.0)]);
    let reports: Vec<String> = opened.reports.lock().unwrap().iter().map(ToString::to_string).collect();
    assert_eq!(reports, ["reported"]);
    // The step after the phase ends the invocation.
    tokio::time::sleep(Duration::from_millis(10)).await;
    let runtime = captured.lock().unwrap().clone().unwrap();
    ended(runtime.snapshot(&RUNTIME_NOTES, root.id, &context()).await);
    ended(runtime.context(root.id, &context(), None).await);
    harness.close(&context()).await.unwrap();
}

/// Hold the line with a blocker commit of the task's conversation until the returned gate is released.
async fn hold_line(storage: &ControlledStorage, root: &Conversation) -> Gate {
    let held = storage.hold_commits();
    let (root, id) = (root.clone(), root.id);
    tokio::spawn(async move {
        let blocker = move |tx: Tx| async move { tx.append_entry(id, EntryDraft::new("blocker")).await };
        let _ = root.commit(blocker, &context()).await;
    });
    held.entered().await;
    held
}

#[tokio::test]
async fn orders_runtime_commits_against_the_step_one_queued_before_it_lands_one_after_it_rejects() {
    let storage = Arc::new(ControlledStorage::default());
    let held: Slot<Gate> = Arc::default();
    let before: Slot<tokio::task::JoinHandle<DurableResult<()>>> = Arc::default();
    let after: Slot<tokio::task::JoinHandle<DurableResult<()>>> = Arc::default();
    let root_slot: Slot<Conversation> = Arc::default();
    let (store, held_slot, before_slot, after_slot, roots) =
        (storage.clone(), held.clone(), before.clone(), after.clone(), root_slot.clone());
    let detached = one_step("test.detached", move |_, runtime, _| {
        let (storage, held_slot, before_slot, after_slot) =
            (store.clone(), held_slot.clone(), before_slot.clone(), after_slot.clone());
        let root = roots.lock().unwrap().clone().unwrap();
        async move {
            // Hold the line, queue a commit without awaiting it, and return; the step queues behind that commit.
            *held_slot.lock().unwrap() = Some(hold_line(&storage, &root).await);
            let first = runtime.clone();
            let commit = async move { first.commit(|_, _| async { Ok(completed(json!("before"))) }, &context()).await };
            *before_slot.lock().unwrap() = Some(tokio::spawn(commit));
            tokio::time::sleep(Duration::from_millis(5)).await;
            // Queued after the handler returned, so after the step.
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(5)).await;
                let commit =
                    async move { runtime.commit(|_, _| async { Ok(completed(json!("after"))) }, &context()).await };
                *after_slot.lock().unwrap() = Some(tokio::spawn(commit));
            });
            Ok(())
        }
    });
    let opened = open_tasks(storage.clone(), vec![detached.clone()]).await;
    let harness = opened.harness;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    *root_slot.lock().unwrap() = Some(root.clone());
    let id = start(&root, &detached, false).await;
    harness.resume().unwrap();
    let after_ready = after.clone();
    eventually(move || after_ready.lock().unwrap().is_some()).await;
    tokio::time::sleep(Duration::from_millis(5)).await;
    held.lock().unwrap().clone().unwrap().release();
    let before = before.lock().unwrap().take().unwrap();
    before.await.unwrap().unwrap();
    assert_eq!(outcome(&harness, id).await, json!({ "status": "completed", "result": "before" }));
    let after = after.lock().unwrap().take().unwrap();
    let error = after.await.unwrap().unwrap_err().to_string();
    assert!(error.contains("invocation has ended") || error.contains("is terminal"), "{error}");
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn stops_a_watch_whose_acquisition_finishes_after_the_invocation_ended() {
    let storage = Arc::new(ControlledStorage::default());
    let held: Slot<Gate> = Arc::default();
    let watching: Slot<tokio::task::JoinHandle<String>> = Arc::default();
    let root_slot: Slot<Conversation> = Arc::default();
    let (store, held_slot, watch_slot, roots) = (storage.clone(), held.clone(), watching.clone(), root_slot.clone());
    let late = one_step("test.late-watch", move |_, runtime, _| {
        let (storage, held_slot, watch_slot) = (store.clone(), held_slot.clone(), watch_slot.clone());
        let root = roots.lock().unwrap().clone().unwrap();
        async move {
            *held_slot.lock().unwrap() = Some(hold_line(&storage, &root).await);
            // Starts after the handler returned, so its line job queues behind the step that ends the invocation.
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(5)).await;
                let watch =
                    async move { runtime.watch_doc(&LATE_NOTES, (), &context()).await.err().unwrap().to_string() };
                *watch_slot.lock().unwrap() = Some(tokio::spawn(watch));
            });
            Ok(())
        }
    });
    let opened = open_tasks(storage.clone(), vec![late.clone()]).await;
    let harness = opened.harness;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    *root_slot.lock().unwrap() = Some(root.clone());
    write_notes(&root, &LATE_NOTES, "x").await;
    let id = start(&root, &late, false).await;
    harness.resume().unwrap();
    let ready = watching.clone();
    eventually(move || ready.lock().unwrap().is_some()).await;
    tokio::time::sleep(Duration::from_millis(5)).await;
    held.lock().unwrap().clone().unwrap().release();
    let watching = watching.lock().unwrap().take().unwrap();
    let error = watching.await.unwrap();
    assert!(error.contains("invocation has ended"), "{error}");
    assert_eq!(outcome(&harness, id).await["status"], "faulted");
    harness.close(&context()).await.unwrap();
}
