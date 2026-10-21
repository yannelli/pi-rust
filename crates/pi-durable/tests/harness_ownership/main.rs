//! Port of upstream `test/harness-ownership.test.ts`. This file holds the helpers and the in-process cascade cases;
//! `marks.rs` holds orphan, cancellation, and dependency races, `reopen.rs` the cases across reopen and failed
//! cascades, and `handles.rs` owned conversations from tools and supervisors.

#[path = "../support/mod.rs"]
mod support;

mod bridges;
mod handles;
mod indexes;
mod marks;
mod reopen;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_chord::Json;
use pi_durable::harness::live::{LIVE_DOC, LiveRun};
use pi_durable::harness::*;
use pi_durable::session::Tx;
use pi_durable::storage::MemoryStorage;
use pi_durable::tasks::TaskOptions;
use pi_durable::*;
use serde_json::{Value, json};
use support::context;
use support::task_support::{aborted, completed, eventually, open_tasks};
use tokio::sync::watch;

/// Outcome a held task commits once its gate opens.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ending {
    Completed,
    Failed,
}

/// Named gates of the `test.hold` tasks, and how often each named task started its run phase.
#[derive(Clone)]
pub struct Board {
    gates: Arc<watch::Sender<HashMap<String, Ending>>>,
    runs: Arc<Mutex<HashMap<String, usize>>>,
}

impl Default for Board {
    fn default() -> Self {
        Self { gates: Arc::new(watch::channel(HashMap::new()).0), runs: Arc::default() }
    }
}

impl Board {
    pub fn open(&self, name: &str, ending: Ending) {
        self.gates.send_modify(|gates| {
            gates.insert(name.to_owned(), ending);
        });
    }

    async fn wait(&self, name: &str) -> Ending {
        let mut receiver = self.gates.subscribe();
        let gates = receiver.wait_for(|gates| gates.contains_key(name)).await.unwrap();
        gates[name]
    }

    pub fn runs(&self, name: &str) -> usize {
        self.runs.lock().unwrap().get(name).copied().unwrap_or_default()
    }
}

pub fn checkpoint(phase: &str) -> Json {
    Json::from(json!({ "phase": phase }))
}

pub fn aborted_state() -> Option<TaskState> {
    Some(TaskState::Terminal { outcome: TaskOutcome::Aborted { reason: None, result: None } })
}

/// A task that holds until its named gate opens or it is aborted. With `slowAbort`, its abort handler first waits for
/// the gate `abort.<name>`.
pub fn hold_task(board: &Board) -> Task {
    let (run_board, abort_board) = (board.clone(), board.clone());
    define_task(
        TaskDefinition::new("test.hold", 1, |_| Ok(checkpoint("hold")))
            .phase("hold", move |task, runtime, context| {
                let board = run_board.clone();
                async move {
                    let name = task.input["name"].as_str().unwrap().to_owned();
                    *board.runs.lock().unwrap().entry(name.clone()).or_default() += 1;
                    let ending = tokio::select! {
                        ending = board.wait(&name) => Some(ending),
                        () = aborted(runtime.signal()) => None,
                    };
                    let state = match ending {
                        Some(Ending::Completed) => completed(Value::Null),
                        _ => Some(TaskState::Terminal {
                            outcome: TaskOutcome::Failed { error: TaskOutcomeError::new("gate failed"), result: None },
                        }),
                    };
                    runtime.commit(move |_, _| async move { Ok(state) }, &context).await
                }
            })
            .abort(move |task, runtime, context| {
                let board = abort_board.clone();
                async move {
                    // Deviation: the slow wait also ends on the signal. Here the abort invocation can start before
                    // close, and close joins invocations; upstream reaches close before dispatching it.
                    if task.input["slowAbort"] == Json::from(true) {
                        let gate = format!("abort.{}", task.input["name"].as_str().unwrap());
                        tokio::select! {
                            _ = board.wait(&gate) => {}
                            () = aborted(runtime.signal()) => return Err(DurableError::aborted()),
                        }
                    }
                    runtime.commit(|_, _| async { Ok(aborted_state()) }, &context).await
                }
            }),
    )
}

/// Waits on the tasks in its input, then completes; its abort handler ends it `aborted`.
pub fn waiter_task() -> Task {
    define_task(
        TaskDefinition::new("test.waiter", 1, |_| Ok(checkpoint("wait")))
            .phase("wait", |task, runtime, context| async move {
                let on: Vec<TaskId> = serde_json::from_value(task.input["on"].to_serde()).unwrap();
                let state = TaskState::Waiting { checkpoint: checkpoint("done"), on, policy: JoinPolicy::AllSettled };
                runtime.commit(move |_, _| async move { Ok(Some(state)) }, &context).await
            })
            .phase("done", |_, runtime, context| async move {
                runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &context).await
            })
            .abort(|_, runtime, context| async move {
                runtime.commit(|_, _| async { Ok(aborted_state()) }, &context).await
            }),
    )
}

pub struct Opened {
    pub harness: Harness,
    pub root: Conversation,
    pub reports: Arc<Mutex<Vec<DurableError>>>,
    pub hold: Task,
}

pub async fn open_harness(storage: Arc<dyn Storage>, board: &Board) -> Opened {
    let hold = hold_task(board);
    let opened = open_tasks(storage, vec![hold.clone(), waiter_task(), bridges::spawner_task(hold.clone())]).await;
    let root = opened.harness.root(&context(), CreateOptions::default()).await.unwrap();
    opened.harness.resume().unwrap();
    Opened { harness: opened.harness, root, reports: opened.reports, hold }
}

pub async fn memory(board: &Board) -> Opened {
    open_harness(Arc::new(MemoryStorage::new()), board).await
}

#[derive(Clone, Copy, Default)]
pub struct ChildOptions {
    pub background: bool,
    pub slow_inner: bool,
    pub slow_owner: bool,
}

pub const BACKGROUND: ChildOptions = ChildOptions { background: true, slow_inner: false, slow_owner: false };

#[derive(Clone, Copy, Debug)]
pub struct Tree {
    pub owner: TaskId,
    pub child: ConversationId,
    pub inner: TaskId,
}

pub fn hold_input(name: &str, slow: bool) -> Json {
    Json::from(if slow { json!({ "name": name, "slowAbort": true }) } else { json!({ "name": name }) })
}

/// In `parent`, stage task `owner` and a conversation it owns holding task `inner`, in one commit.
pub async fn owned_child(parent: &Conversation, hold: &Task, name: &str, options: ChildOptions) -> Tree {
    let (hold, name, parent_id) = (hold.clone(), name.to_owned(), parent.id);
    let stage = move |tx: Tx| async move {
        let owned = TaskOptions { background: options.background, ..TaskOptions::conversation(parent_id) };
        let owner = tx.create_task(hold.clone(), hold_input(&name, options.slow_owner), owned).await?;
        let child = tx.create_conversation(ConversationOwnership::Task { task_id: owner }).await?;
        let input = hold_input(&format!("{name}.inner"), options.slow_inner);
        let inner = tx.create_task(hold, input, TaskOptions::conversation(child.id)).await?;
        Ok(Tree { owner, child: child.id, inner })
    };
    parent.commit(stage, &context()).await.unwrap()
}

pub async fn create_hold(conversation: &Conversation, hold: &Task, name: &str) -> TaskId {
    let (hold, input, id) = (hold.clone(), hold_input(name, false), conversation.id);
    let create = move |tx: Tx| async move { tx.create_task(hold, input, TaskOptions::conversation(id)).await };
    conversation.commit(create, &context()).await.unwrap()
}

/// Mark a conversation busy with `task` standing in for its run, so submissions queue.
pub async fn busy(conversation: &Conversation, task: TaskId) {
    let id = conversation.id;
    let mark = move |tx: Tx| async move {
        tx.doc(&LIVE_DOC, id).await?.edit(|live| live.run = Some(LiveRun { task_id: task, inputs: Vec::new() }))
    };
    conversation.commit(mark, &context()).await.unwrap();
}

pub async fn status(harness: &Harness, id: TaskId) -> TaskStatus {
    harness.get_task(id, &context()).await.unwrap().unwrap().state.status()
}

pub async fn marked(harness: &Harness, id: TaskId) -> bool {
    harness.get_task(id, &context()).await.unwrap().unwrap().abort_requested
}

/// The outcome status `id` settles with.
pub async fn settles(harness: &Harness, id: TaskId) -> &'static str {
    harness.wait_for_task(id, &context()).await.unwrap().state.outcome().unwrap().status()
}

pub async fn child(harness: &Harness, id: ConversationId) -> Conversation {
    harness.conversation(id, &context()).await.unwrap().unwrap()
}

pub async fn submission(harness: &Harness, id: SubmissionId) -> SubmissionRecord {
    harness.submission(id, &context()).await.unwrap().unwrap().status(&context()).await.unwrap()
}

pub async fn input(conversation: &Conversation, text: &str) -> SubmissionId {
    conversation.submit(SubmissionDraft::input(text), &context()).await.unwrap().id
}

/// Whether the idle wait of `conversation` (or of the Harness without one) settles within a few scheduler turns.
pub async fn settled(harness: &Harness, conversation: Option<&Conversation>) -> bool {
    let (harness, conversation) = (harness.clone(), conversation.cloned());
    let wait = tokio::spawn(async move {
        match conversation {
            Some(conversation) => conversation.wait_for_idle(&context()).await,
            None => harness.wait_for_idle(&context()).await,
        }
    });
    tokio::time::timeout(Duration::from_millis(20), wait).await.is_ok()
}

#[tokio::test]
async fn keeps_a_conversation_busy_while_its_owned_foreground_subtree_has_live_work_holding_the_completed_owner() {
    let board = Board::default();
    let Opened { harness, root, hold, .. } = memory(&board).await;
    let tree = owned_child(&root, &hold, "owner", ChildOptions::default()).await;
    board.open("owner", Ending::Completed);
    eventually(|| async { status(&harness, tree.owner).await == TaskStatus::Completing }).await;
    let waiter = root.clone();
    let idle = tokio::spawn(async move { waiter.wait_for_idle(&context()).await });
    assert!(!settled(&harness, Some(&root)).await);
    assert!(!settled(&harness, None).await);
    board.open("owner.inner", Ending::Completed);
    idle.await.unwrap().unwrap();
    harness.wait_for_idle(&context()).await.unwrap();
    assert_eq!(settles(&harness, tree.owner).await, "completed");
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn stops_idle_traversal_at_a_background_owner() {
    let board = Board::default();
    let Opened { harness, root, hold, .. } = memory(&board).await;
    let tree = owned_child(&root, &hold, "background", BACKGROUND).await;
    root.wait_for_idle(&context()).await.unwrap();
    harness.wait_for_idle(&context()).await.unwrap();
    // The background child is its own scope.
    let child = child(&harness, tree.child).await;
    assert!(!settled(&harness, Some(&child)).await);
    child.abort(&context(), false).await.unwrap();
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn cascades_an_abort_mark_to_the_owned_foreground_subtree_and_withdraws_its_queued_inputs() {
    let board = Board::default();
    let Opened { harness, root, hold, .. } = memory(&board).await;
    let tree = owned_child(&root, &hold, "owner", ChildOptions::default()).await;
    let nested = child(&harness, tree.child).await;
    let deeper = owned_child(&nested, &hold, "deeper", ChildOptions::default()).await;
    let shielded = owned_child(&nested, &hold, "shielded", BACKGROUND).await;
    // Busy conversations queue submissions; the inner task stands in for the child's run.
    busy(&nested, tree.inner).await;
    let queued = input(&nested, "later").await;
    assert_eq!(harness.abort_task(tree.owner, &context()).await.unwrap(), AbortTaskResult::Marked);
    for id in [tree.owner, tree.inner, deeper.owner, deeper.inner] {
        assert_eq!(settles(&harness, id).await, "aborted");
    }
    // A nested background owner is a boundary.
    assert_ne!(status(&harness, shielded.owner).await, TaskStatus::Terminal);
    assert_ne!(status(&harness, shielded.inner).await, TaskStatus::Terminal);
    let record = submission(&harness, queued).await;
    assert_eq!((record.status, record.reason.as_deref()), (SubmissionStatus::Unanswered, Some("aborted")));
    harness.abort_task(shielded.owner, &context()).await.unwrap();
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn cascades_a_failed_owner_but_not_a_completed_one() {
    let board = Board::default();
    let Opened { harness, root, hold, .. } = memory(&board).await;
    let failed = owned_child(&root, &hold, "failed", ChildOptions::default()).await;
    let done = owned_child(&root, &hold, "done", ChildOptions::default()).await;
    board.open("failed", Ending::Failed);
    board.open("done", Ending::Completed);
    assert_eq!(settles(&harness, failed.inner).await, "aborted");
    assert_eq!(settles(&harness, failed.owner).await, "failed");
    eventually(|| async { status(&harness, done.owner).await == TaskStatus::Completing }).await;
    assert_ne!(status(&harness, done.inner).await, TaskStatus::Terminal);
    board.open("done.inner", Ending::Completed);
    assert_eq!(settles(&harness, done.owner).await, "completed");
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn aborts_a_background_task_directly_with_its_ordinary_subtree() {
    let board = Board::default();
    let Opened { harness, root, hold, .. } = memory(&board).await;
    let tree = owned_child(&root, &hold, "background", BACKGROUND).await;
    harness.abort_task(tree.owner, &context()).await.unwrap();
    assert_eq!(settles(&harness, tree.inner).await, "aborted");
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn aborts_a_conversation_queued_inputs_withdrawn_writes_kept_foreground_work_aborted_background_kept() {
    let board = Board::default();
    let Opened { harness, root, hold, .. } = memory(&board).await;
    let foreground = owned_child(&root, &hold, "foreground", ChildOptions::default()).await;
    let background = owned_child(&root, &hold, "background", BACKGROUND).await;
    busy(&root, foreground.owner).await;
    let queued = input(&root, "later").await;
    let write = root.submit(SubmissionDraft::write(EntryDraft::new("note")), &context()).await.unwrap().id;
    root.abort(&context(), false).await.unwrap();
    for id in [foreground.owner, foreground.inner] {
        assert_eq!(status(&harness, id).await, TaskStatus::Terminal);
    }
    assert_ne!(status(&harness, background.owner).await, TaskStatus::Terminal);
    assert_ne!(status(&harness, background.inner).await, TaskStatus::Terminal);
    assert_eq!(submission(&harness, queued).await.reason.as_deref(), Some("aborted"));
    assert_eq!(submission(&harness, write).await.status, SubmissionStatus::Queued);
    harness.abort_task(background.owner, &context()).await.unwrap();
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn aborts_work_created_below_a_held_failed_owner_but_not_below_a_terminal_one() {
    let board = Board::default();
    let Opened { harness, root, hold, .. } = memory(&board).await;
    let tree = owned_child(&root, &hold, "owner", ChildOptions { slow_inner: true, ..Default::default() }).await;
    board.open("owner", Ending::Failed);
    eventually(|| marked(&harness, tree.inner)).await;
    assert_eq!(status(&harness, tree.owner).await, TaskStatus::Completing);
    let child = child(&harness, tree.child).await;
    let during = create_hold(&child, &hold, "during").await;
    assert_eq!(settles(&harness, during).await, "aborted");
    board.open("abort.owner.inner", Ending::Completed);
    assert_eq!(settles(&harness, tree.owner).await, "failed");
    // A terminal owner never cascades: interrogating its conversation runs normally.
    let after = create_hold(&child, &hold, "after").await;
    board.open("after", Ending::Completed);
    assert_eq!(settles(&harness, after).await, "completed");
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn withdraws_queued_inputs_below_the_aborted_task_but_keeps_its_own_conversation_s_queue_and_queued_writes() {
    let board = Board::default();
    let Opened { harness, root, hold, .. } = memory(&board).await;
    let tree = owned_child(&root, &hold, "owner", ChildOptions::default()).await;
    let child = child(&harness, tree.child).await;
    busy(&root, tree.owner).await;
    busy(&child, tree.inner).await;
    let own = input(&root, "own").await;
    let below = input(&child, "below").await;
    let write = child.submit(SubmissionDraft::write(EntryDraft::new("note")), &context()).await.unwrap().id;
    harness.abort_task(tree.owner, &context()).await.unwrap();
    harness.wait_for_task(tree.inner, &context()).await.unwrap();
    assert_eq!(submission(&harness, own).await.status, SubmissionStatus::Queued);
    assert_eq!(submission(&harness, below).await.status, SubmissionStatus::Unanswered);
    assert_eq!(submission(&harness, write).await.status, SubmissionStatus::Queued);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn keeps_a_nested_background_owner_s_subtree_when_its_cancelled_background_owner_is_aborted() {
    let board = Board::default();
    let Opened { harness, root, hold, .. } = memory(&board).await;
    let outer = owned_child(&root, &hold, "outer", BACKGROUND).await;
    let inner = owned_child(&child(&harness, outer.child).await, &hold, "inner", BACKGROUND).await;
    harness.abort_task(outer.owner, &context()).await.unwrap();
    assert_eq!(settles(&harness, outer.inner).await, "aborted");
    assert_ne!(status(&harness, inner.owner).await, TaskStatus::Terminal);
    assert_ne!(status(&harness, inner.inner).await, TaskStatus::Terminal);
    harness.abort_task(inner.owner, &context()).await.unwrap();
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn withdraws_an_input_queued_below_a_held_failed_owner_after_its_cascade() {
    let board = Board::default();
    let Opened { harness, root, hold, .. } = memory(&board).await;
    let tree = owned_child(&root, &hold, "owner", ChildOptions { slow_inner: true, ..Default::default() }).await;
    let child = child(&harness, tree.child).await;
    // The inner task stands in for the child's run, which stays busy after the cascade.
    busy(&child, tree.inner).await;
    board.open("owner", Ending::Failed);
    eventually(|| marked(&harness, tree.inner)).await;
    let late = child.submit(SubmissionDraft::input("late"), &context()).await.unwrap();
    let record = late.wait(&context()).await.unwrap();
    assert_eq!((record.status, record.reason.as_deref()), (SubmissionStatus::Unanswered, Some("aborted")));
    board.open("abort.owner.inner", Ending::Completed);
    harness.wait_for_task(tree.owner, &context()).await.unwrap();
    harness.close(&context()).await.unwrap();
}
