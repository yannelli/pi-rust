//! Scheduler memory is bounded by live work: an ended task or a conversation edge is kept only while live work is below
//! it, and a chain dropped that way is loaded again from storage when work appears below it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pi_chord::Json;
use pi_durable::harness::IndexSizes;
use pi_durable::session::Tx;
use pi_durable::tasks::TaskOptions;
use pi_durable::*;
use serde_json::json;

use super::support::task_support::{aborted, completed, eventually, open_tasks};
use super::support::{ControlledStorage, Deferred, StorageCall, context};
use super::{
    Board, Ending, checkpoint, child, create_hold, hold_input, hold_task, input, marked, settled, settles, status,
    waiter_task,
};
use pi_durable::harness::{Conversation, CreateOptions, Harness, Task, TaskDefinition, define_task};

/// Holds until its gate opens, then creates a held task `inside.<name>` in conversation `input.into` and ends.
fn spawner_task(board: &Board, hold: &Task) -> Task {
    let (board, hold) = (board.clone(), hold.clone());
    define_task(
        TaskDefinition::new("test.retention.spawner", 1, |_| Ok(checkpoint("hold")))
            .phase("hold", move |task, runtime, context| {
                let (board, hold) = (board.clone(), hold.clone());
                async move {
                    let name = task.input["name"].as_str().unwrap().to_owned();
                    tokio::select! {
                        _ = board.wait(&name) => {}
                        () = aborted(runtime.signal()) => return Ok(()),
                    }
                    // Read now: the test points the task at its conversation after creating it.
                    let current = runtime.get_task(task.id, &context).await?.unwrap();
                    let into = ConversationId(current.input["into"].as_u64().unwrap());
                    let create = move |tx: Tx, _| async move {
                        let inside = hold_input(&format!("inside.{name}"), false);
                        tx.create_task(hold, inside, TaskOptions::conversation(into)).await?;
                        Ok(completed(json!(null)))
                    };
                    runtime.commit(create, &context).await
                }
            })
            .abort(|_, runtime, context| async move {
                runtime.commit(|_, _| async { Ok(super::aborted_state()) }, &context).await
            }),
    )
}

type Rejections = Arc<Mutex<Vec<String>>>;

/// Holds until `rejected` opens, then tries one terminal commit that creates a task in conversation `into` and a child
/// of its own, which the Transaction rejects as the task is ending. Catches the rejection and holds.
fn rejected_task(board: &Board, hold: &Task, rejections: &Rejections) -> Task {
    let (board, hold, rejections) = (board.clone(), hold.clone(), rejections.clone());
    define_task(
        TaskDefinition::new("test.retention.rejected", 1, |_| Ok(checkpoint("hold")))
            .phase("hold", move |task, runtime, context| {
                let (board, hold, rejections) = (board.clone(), hold.clone(), rejections.clone());
                async move {
                    tokio::select! {
                        _ = board.wait("rejected") => {}
                        () = aborted(runtime.signal()) => return Ok(()),
                    }
                    let into = ConversationId(task.input["into"].as_u64().unwrap());
                    let owner = task.id;
                    let create = move |tx: Tx, _| async move {
                        let staged = hold_input("staged", false);
                        tx.create_task(hold.clone(), staged, TaskOptions::conversation(into)).await?;
                        tx.create_task(hold, hold_input("child", false), TaskOptions::owned_by(owner)).await?;
                        Ok(completed(json!(null)))
                    };
                    if let Err(error) = runtime.commit(create, &context).await {
                        rejections.lock().unwrap().push(error.to_string());
                    }
                    aborted(runtime.signal()).await;
                    Ok(())
                }
            })
            .abort(|_, runtime, context| async move {
                runtime.commit(|_, _| async { Ok(super::aborted_state()) }, &context).await
            }),
    )
}

pub struct Env {
    pub harness: Harness,
    pub root: Conversation,
    pub hold: Task,
    pub spawner: Task,
    pub rejected: Task,
    pub rejections: Rejections,
}

pub async fn open(storage: Arc<ControlledStorage>, board: &Board) -> Env {
    let hold = hold_task(board);
    let rejections = Rejections::default();
    let (spawner, rejected) = (spawner_task(board, &hold), rejected_task(board, &hold, &rejections));
    let tasks = vec![hold.clone(), waiter_task(), spawner.clone(), rejected.clone()];
    let opened = open_tasks(storage, tasks).await;
    let root = opened.harness.root(&context(), CreateOptions::default()).await.unwrap();
    opened.harness.resume().unwrap();
    Env { harness: opened.harness, root, hold, spawner, rejected, rejections }
}

pub fn sizes(harness: &Harness) -> IndexSizes {
    harness.scheduler_index_sizes()
}

/// Count the task and conversation reads a storage serves.
fn count_reads(storage: &ControlledStorage) -> Arc<AtomicUsize> {
    let reads = Arc::new(AtomicUsize::new(0));
    let counter = reads.clone();
    *storage.call_hook.lock().unwrap() = Some(Arc::new(move |call: StorageCall| {
        if call.method == "task" || call.method == "conversation" {
            counter.fetch_add(1, Ordering::SeqCst);
        }
        Box::pin(async { Ok(()) })
    }));
    reads
}

/// Under live `parent`: tool task `T` owning conversation `C`, a task in `C`; all of them end.
pub async fn subagent_cycle(env: &Env, board: &Board, parent: TaskId, n: usize) {
    let hold = env.hold.clone();
    let stage = move |tx: Tx| async move {
        let tool = hold_input(&format!("tool{n}"), false);
        let tool = tx.create_task(hold.clone(), tool, TaskOptions::owned_by(parent)).await?;
        let created = tx.create_conversation(ConversationOwnership::Task { task_id: tool }).await?;
        let inner = hold_input(&format!("inner{n}"), false);
        let inner = tx.create_task(hold, inner, TaskOptions::conversation(created.id)).await?;
        Ok((tool, inner))
    };
    let (tool, inner) = env.root.commit(stage, &context()).await.unwrap();
    board.open(&format!("inner{n}"), Ending::Completed);
    board.open(&format!("tool{n}"), Ending::Completed);
    env.harness.wait_for_task(inner, &context()).await.unwrap();
    env.harness.wait_for_task(tool, &context()).await.unwrap();
}

/// Live `p` owns `b`, which owns `a`, which owns the empty conversation `c`; `b` and `a` end and are dropped.
pub struct Dropped {
    pub p: TaskId,
    pub b: TaskId,
    pub c: ConversationId,
}

pub async fn drop_chain(env: &Env, first: Task, input: Json) -> Dropped {
    let (id, hold) = (env.root.id, env.hold.clone());
    let stage = move |tx: Tx| async move {
        let p = tx.create_task(first, input, TaskOptions::conversation(id)).await?;
        let b = tx.create_task(hold.clone(), hold_input("b", false), TaskOptions::owned_by(p)).await?;
        let a = tx.create_task(hold, hold_input("a", false), TaskOptions::owned_by(b)).await?;
        let c = tx.create_conversation(ConversationOwnership::Task { task_id: a }).await?;
        Ok(Dropped { p, b, c: c.id })
    };
    env.root.commit(stage, &context()).await.unwrap()
}

#[tokio::test]
async fn forgets_ended_subagent_chains_once_their_work_ends() {
    let board = Board::default();
    let env = open(Arc::new(ControlledStorage::default()), &board).await;
    let parent = create_hold(&env.root, &env.hold, "parent").await;
    subagent_cycle(&env, &board, parent, 0).await;
    let baseline = sizes(&env.harness);
    for n in 1..=200 {
        subagent_cycle(&env, &board, parent, n).await;
    }
    assert_eq!(sizes(&env.harness), baseline);
    board.open("parent", Ending::Completed);
    settles(&env.harness, parent).await;
    env.harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn holds_a_live_owner_for_work_later_created_in_a_forgotten_conversation_loading_its_chain_again() {
    let storage = Arc::new(ControlledStorage::default());
    let reads = count_reads(&storage);
    let board = Board::default();
    let env = open(storage, &board).await;
    let tree = drop_chain(&env, env.hold.clone(), hold_input("p", false)).await;
    board.open("a", Ending::Completed);
    board.open("b", Ending::Completed);
    settles(&env.harness, tree.b).await;
    let before = sizes(&env.harness);
    let loaded = reads.load(Ordering::SeqCst);
    let d = create_hold(&child(&env.harness, tree.c).await, &env.hold, "d").await;
    board.open("p", Ending::Completed);
    eventually(|| async { status(&env.harness, tree.p).await == TaskStatus::Completing }).await;
    assert!(reads.load(Ordering::SeqCst) > loaded);
    board.open("d", Ending::Completed);
    assert_eq!(settles(&env.harness, d).await, "completed");
    assert_eq!(settles(&env.harness, tree.p).await, "completed");
    assert!(sizes(&env.harness).settled <= before.settled);
    env.harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn withdraws_an_input_queued_in_a_forgotten_conversation_below_an_abort_marked_owner() {
    let board = Board::default();
    let env = open(Arc::new(ControlledStorage::default()), &board).await;
    // Live p -> ended b -> conversation c with no task; c gets a queued input while p is cancelling.
    let (id, hold) = (env.root.id, env.hold.clone());
    let stage = move |tx: Tx| async move {
        let p = tx.create_task(hold.clone(), hold_input("p", false), TaskOptions::conversation(id)).await?;
        let b = tx.create_task(hold.clone(), hold_input("b", false), TaskOptions::owned_by(p)).await?;
        let c = tx.create_conversation(ConversationOwnership::Task { task_id: b }).await?;
        // A live child keeps p holding its abort until the input is withdrawn and the child ends.
        let keep = tx.create_task(hold, hold_input("keep", false), TaskOptions::owned_by(p)).await?;
        Ok((p, b, c.id, keep))
    };
    let (p, b, c, keep) = env.root.commit(stage, &context()).await.unwrap();
    board.open("b", Ending::Completed);
    settles(&env.harness, b).await;
    let c = child(&env.harness, c).await;
    // Busy, so the input queues instead of starting a run.
    super::busy(&c, keep).await;
    let queued = input(&c, "later").await;
    env.harness.abort_task(p, &context()).await.unwrap();
    eventually(|| async { super::submission(&env.harness, queued).await.status == SubmissionStatus::Unanswered }).await;
    assert_eq!(super::submission(&env.harness, queued).await.reason.as_deref(), Some("aborted"));
    settles(&env.harness, p).await;
    env.harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn aborts_work_created_in_a_forgotten_conversation_below_an_abort_marked_owner_before_it_runs() {
    let board = Board::default();
    let env = open(Arc::new(ControlledStorage::default()), &board).await;
    let (id, hold) = (env.root.id, env.hold.clone());
    let stage = move |tx: Tx| async move {
        let p = tx.create_task(hold.clone(), hold_input("p", false), TaskOptions::conversation(id)).await?;
        let b = tx.create_task(hold.clone(), hold_input("b", false), TaskOptions::owned_by(p)).await?;
        let c = tx.create_conversation(ConversationOwnership::Task { task_id: b }).await?;
        // Deviation: a slow abort handler keeps p holding until the late task exists; upstream relies on timing.
        let keep = tx.create_task(hold, hold_input("keep", true), TaskOptions::owned_by(p)).await?;
        Ok((p, b, c.id, keep))
    };
    let (p, b, c, keep) = env.root.commit(stage, &context()).await.unwrap();
    board.open("b", Ending::Completed);
    settles(&env.harness, b).await;
    // The task p cancels; its live child keeps it holding.
    let mark = move |tx: Tx| async move {
        let record = tx.task(p).await?.unwrap();
        tx.set_task(TaskRecord { abort_requested: true, ..record })
    };
    env.harness.commit(mark, &context()).await.unwrap();
    eventually(|| marked(&env.harness, keep)).await;
    let late = create_hold(&child(&env.harness, c).await, &env.hold, "late").await;
    assert_eq!(settles(&env.harness, late).await, "aborted");
    assert_eq!(board.runs("late"), 0);
    board.open("abort.keep", Ending::Completed);
    settles(&env.harness, p).await;
    env.harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn keeps_a_scope_busy_while_the_chain_of_new_work_below_a_forgotten_conversation_loads() {
    let storage = Arc::new(ControlledStorage::default());
    let board = Board::default();
    let env = open(storage.clone(), &board).await;
    let (id, hold) = (env.root.id, env.hold.clone());
    let stage = move |tx: Tx| async move {
        let b = tx.create_task(hold, hold_input("b", false), TaskOptions::conversation(id)).await?;
        Ok((b, tx.create_conversation(ConversationOwnership::Task { task_id: b }).await?.id))
    };
    let (b, c) = env.root.commit(stage, &context()).await.unwrap();
    board.open("b", Ending::Completed);
    settles(&env.harness, b).await;
    // Hold the chain load's read of c: the second read once holding, after the creating commit's own.
    let (held, reached, holding) = (Deferred::default(), Deferred::default(), Arc::new(AtomicUsize::new(0)));
    let (gate, signal, reads) = (held.clone(), reached.clone(), holding.clone());
    *storage.call_hook.lock().unwrap() = Some(Arc::new(move |call: StorageCall| {
        let hold = call.method == "conversation" && call.id == Some(c.0) && reads.load(Ordering::SeqCst) > 0;
        let (gate, signal, reads) = (gate.clone(), signal.clone(), reads.clone());
        Box::pin(async move {
            if hold && reads.fetch_add(1, Ordering::SeqCst) == 2 {
                signal.resolve();
                gate.wait().await;
            }
            Ok(())
        })
    }));
    let conversation = child(&env.harness, c).await;
    holding.store(1, Ordering::SeqCst);
    create_hold(&conversation, &env.hold, "d").await;
    reached.wait().await;
    assert!(!settled(&env.harness, None).await);
    held.resolve();
    board.open("d", Ending::Completed);
    env.harness.wait_for_idle(&context()).await.unwrap();
    env.harness.close(&context()).await.unwrap();
}
