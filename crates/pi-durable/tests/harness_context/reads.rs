//! The upstream cases of task context reads: extension with newer entries, keeping a read across tasks, and dropping
//! it once the conversation is idle.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

use pi_durable::tasks::TaskOptions;
use pi_durable::{JoinPolicy, TaskId, TaskRecord, TaskState};
use serde_json::Value;

use super::support::ControlledStorage;
use super::support::task_support::{completed, open_tasks_with};
use super::*;

pub type Rows = Arc<Mutex<BTreeMap<String, usize>>>;

pub struct Counting {
    pub storage: Arc<ControlledStorage>,
    pub harness: Harness,
    pub root: Conversation,
    pub first: EntryRecord,
}

impl Counting {
    /// Create a task of the root conversation with input `{ name }` and wait for it to complete.
    pub async fn run(&self, task: &Task, name: &str) {
        let (task, id, input) = (task.clone(), self.root.id, Json::from(json!({ "name": name })));
        let create = move |tx: Tx| async move { tx.create_task(task, input, TaskOptions::conversation(id)).await };
        let id: TaskId = self.root.commit(create, &context()).await.unwrap();
        let record = self.harness.wait_for_task(id, &context()).await.unwrap();
        assert_eq!(serde_json::to_value(record.state.outcome()).unwrap()["status"], "completed");
    }
}

/// A Harness over counting storage holding `tasks`, with a 21-entry root transcript.
pub async fn counting_setup(tasks: Vec<Task>, configure: impl FnOnce(HarnessOptions) -> HarnessOptions) -> Counting {
    let storage = Arc::new(ControlledStorage::default());
    let registry = Arc::new(create_registry());
    let harness = open_tasks_with(storage.clone(), tasks, registry, configure).await.harness;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    let first = append(&root, root.id, EntryDraft { model: Some(vec![user("first")]), ..EntryDraft::new("message") });
    let first = first.await;
    for index in 0..20 {
        let draft = EntryDraft {
            model: Some(vec![assistant(&format!("old {index}"), StopReason::Stop)]),
            ..EntryDraft::new("message")
        };
        append(&root, root.id, draft).await;
    }
    Counting { storage, harness, root, first }
}

async fn append(root: &Conversation, id: pi_durable::ConversationId, draft: EntryDraft) -> EntryRecord {
    root.commit(move |tx: Tx| async move { tx.append_entry(id, draft).await }, &context()).await.unwrap()
}

/// A task whose single phase runs `read` with its runtime, context, and input name, then completes.
pub fn probe_task<F, Fut>(name: &str, read: F) -> Task
where
    F: Fn(String, TaskRuntime, pi_chord::Context) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let read = Arc::new(read);
    define_task(
        TaskDefinition::new(name, 1, |_| Ok(Json::from(json!({ "phase": "run" }))))
            .phase("run", move |task, runtime, task_context| {
                let read = read.clone();
                let name = task.input.get("name").and_then(Json::as_str).unwrap_or_default().to_owned();
                async move {
                    read(name, runtime.clone(), task_context.clone()).await;
                    runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &task_context).await
                }
            })
            .abort(|_, _, _| async { Ok(()) }),
    )
}

/// Write `draft` to the root conversation from a task, with no state change.
async fn write(
    runtime: &TaskRuntime,
    id: pi_durable::ConversationId,
    draft: EntryDraft,
    task_context: &pi_chord::Context,
) {
    let change = move |tx: Tx, _: TaskRecord| async move {
        tx.append_entry(id, draft).await?;
        Ok(None)
    };
    runtime.commit(change, task_context).await.unwrap();
}

#[tokio::test]
async fn extends_a_tasks_context_read_with_only_newer_entries() {
    type Slot = Arc<Mutex<Option<(Arc<ControlledStorage>, Conversation, EntryId)>>>;
    let slot: Slot = Arc::default();
    let shared = slot.clone();
    let reads = probe_task("test.context-reads", move |_, runtime, task_context| {
        let (storage, root, first) = shared.lock().unwrap().clone().unwrap();
        async move {
            let id = root.id;
            let read = |at: Option<EntryId>| {
                let (runtime, storage, task_context) = (runtime.clone(), storage.clone(), task_context.clone());
                async move {
                    let before = storage.scanned_rows.load(Ordering::SeqCst);
                    let view = runtime.context(id, &task_context, at).await.unwrap();
                    (view, storage.scanned_rows.load(Ordering::SeqCst) - before)
                }
            };
            let whole = || async { root.context(&task_context, None).await.unwrap() };
            let (initial, _) = read(None).await;
            assert_eq!(initial, whole().await);

            // Three new entries: the bounds probe reads one row, the extension the three new ones.
            write(
                &runtime,
                id,
                EntryDraft { model: Some(vec![user("new")]), ..EntryDraft::new("message") },
                &task_context,
            )
            .await;
            let note =
                EntryDraft { data: Some(Json::from(json!({ "text": "display only" }))), ..EntryDraft::new("note") };
            write(&runtime, id, note, &task_context).await;
            let answer =
                EntryDraft { model: Some(vec![assistant("answer", StopReason::Stop)]), ..EntryDraft::new("message") };
            write(&runtime, id, answer, &task_context).await;
            let (extended, rows) = read(None).await;
            assert_eq!(rows, 4);
            assert_eq!(extended, whole().await);

            // A newer edit of an older entry applies to the extended range.
            let edit = EntryDraft { edits: Some(vec![ContextEdit::omit(first)]), ..EntryDraft::new("edit") };
            write(&runtime, id, edit, &task_context).await;
            let (edited, rows) = read(None).await;
            assert_eq!(rows, 2);
            assert_eq!(edited, whole().await);
            assert!(!described(&edited).contains(&"user:first".to_owned()));

            // An earlier cutoff reuses the range.
            let (cutoff, rows) = read(Some(extended.entries.last().unwrap().id)).await;
            assert_eq!(rows, 0);
            assert_eq!(cutoff, extended);

            // A new head marker changes the range: read it whole.
            let reset = EntryDraft {
                head: Some(DraftHead::SelfEntry),
                model: Some(vec![user("fresh")]),
                ..EntryDraft::new("reset")
            };
            write(&runtime, id, reset, &task_context).await;
            let (reset, _) = read(None).await;
            assert_eq!(reset, whole().await);
            assert_eq!(described(&reset), ["user:fresh"]);
        }
    });
    let counting = counting_setup(vec![reads.clone()], |options| options).await;
    *slot.lock().unwrap() = Some((counting.storage.clone(), counting.root.clone(), counting.first.id));
    counting.harness.resume().unwrap();
    counting.run(&reads, "reads").await;
    counting.harness.close(&context()).await.unwrap();
}

type Shared = Arc<Mutex<Option<(Arc<ControlledStorage>, Conversation)>>>;

/// Read the root context, record the rows it scanned under `name`, and compare it with a whole read.
async fn record_read(
    shared: &Shared,
    rows: &Rows,
    name: &str,
    runtime: &TaskRuntime,
    task_context: &pi_chord::Context,
) {
    let (storage, root) = shared.lock().unwrap().clone().unwrap();
    let before = storage.scanned_rows.load(Ordering::SeqCst);
    let view = runtime.context(root.id, task_context, None).await.unwrap();
    rows.lock().unwrap().insert(name.to_owned(), storage.scanned_rows.load(Ordering::SeqCst) - before);
    assert_eq!(view, root.context(task_context, None).await.unwrap());
}

#[tokio::test]
async fn keeps_a_conversations_context_read_across_its_tasks_and_for_the_retention_period_once_idle() {
    let clock = Arc::new(Mutex::new(1_000.0));
    let rows: Rows = Arc::default();
    let shared: Shared = Arc::default();
    let (child_rows, child_shared) = (rows.clone(), shared.clone());
    let child = probe_task("test.context-child", move |_, runtime, task_context| {
        let (rows, shared) = (child_rows.clone(), child_shared.clone());
        async move {
            let id = runtime.conversation_id();
            let draft = EntryDraft { model: Some(vec![user("from child")]), ..EntryDraft::new("message") };
            write(&runtime, id, draft, &task_context).await;
            record_read(&shared, &rows, "child", &runtime, &task_context).await;
        }
    });
    let (start_rows, start_shared, after_rows, after_shared) =
        (rows.clone(), shared.clone(), rows.clone(), shared.clone());
    let child_task = child.clone();
    let parent = define_task(
        TaskDefinition::new("test.context-parent", 1, |_| Ok(Json::from(json!({ "phase": "start" }))))
            .phase("start", move |_, runtime, task_context| {
                let (rows, shared, child) = (start_rows.clone(), start_shared.clone(), child_task.clone());
                async move {
                    record_read(&shared, &rows, "parent", &runtime, &task_context).await;
                    let (owner, id) = (runtime.task_id(), runtime.conversation_id());
                    let change = move |tx: Tx, _| async move {
                        let options = TaskOptions { conversation_id: Some(id), ..TaskOptions::owned_by(owner) };
                        let child = tx.create_task(child, Json::from(json!({})), options).await?;
                        let checkpoint = Json::from(json!({ "phase": "after" }));
                        Ok(Some(TaskState::Waiting { checkpoint, on: vec![child], policy: JoinPolicy::AllSettled }))
                    };
                    runtime.commit(change, &task_context).await
                }
            })
            .phase("after", move |_, runtime, task_context| {
                let (rows, shared) = (after_rows.clone(), after_shared.clone());
                async move {
                    record_read(&shared, &rows, "after", &runtime, &task_context).await;
                    runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &task_context).await
                }
            })
            .abort(|_, _, _| async { Ok(()) }),
    );
    let (probe_rows, probe_shared) = (rows.clone(), shared.clone());
    let probe = probe_task("test.context-probe", move |name, runtime, task_context| {
        let (rows, shared) = (probe_rows.clone(), probe_shared.clone());
        async move { record_read(&shared, &rows, &name, &runtime, &task_context).await }
    });
    let now = clock.clone();
    let tasks = vec![child, parent.clone(), probe.clone()];
    let counting = counting_setup(tasks, |options| options.with_now(move || *now.lock().unwrap())).await;
    *shared.lock().unwrap() = Some((counting.storage.clone(), counting.root.clone()));
    counting.harness.resume().unwrap();
    counting.run(&parent, "parent").await;
    counting.root.wait_for_idle(&context()).await.unwrap();
    counting.run(&probe, "idle").await;
    counting.root.wait_for_idle(&context()).await.unwrap();
    *clock.lock().unwrap() += 600_000.0;
    counting.run(&probe, "expired").await;
    // Bounds probes read one row each. The child scans its own new entry; the waiting parent's later invocation and
    // the probe within the retention period scan nothing new. After it, the probe reads the whole transcript again.
    let expected = [("after", 1), ("child", 2), ("expired", 23), ("idle", 1), ("parent", 22)];
    assert_eq!(*rows.lock().unwrap(), BTreeMap::from(expected.map(|(name, rows)| (name.to_owned(), rows))));
    counting.harness.close(&context()).await.unwrap();
}

/// Deterministic pseudo-random numbers (32-bit LCG, high bits), as upstream draws them.
pub struct Lcg(pub u32);

impl Lcg {
    pub fn below(&mut self, n: usize) -> usize {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        ((self.0 as u64 * n as u64) >> 32) as usize
    }
}
