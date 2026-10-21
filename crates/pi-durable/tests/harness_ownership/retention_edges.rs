//! Retention edge cases: a chain kept through one publication, work dropped by a sweep job, and deep chains.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use pi_chord::Json;
use pi_durable::session::Tx;
use pi_durable::tasks::TaskOptions;
use pi_durable::*;
use serde_json::json;

use super::retention::{Env, drop_chain, open, sizes};
use super::support::task_support::{completed, eventually};
use super::support::{ControlledStorage, StorageCall, context};
use super::{Board, Ending, busy, child, create_hold, hold_input, input, settles, status, submission};

#[tokio::test]
async fn holds_an_owner_for_work_its_own_terminal_commit_creates_below_a_forgotten_chain() {
    let board = Board::default();
    let env = open(Arc::new(ControlledStorage::default()), &board).await;
    // Live p (a spawner) -> ended b -> ended a -> conversation c, empty: forgotten. The terminal commit of p creates
    // a task in c.
    let tree = drop_chain(&env, env.spawner.clone(), Json::from(json!({ "name": "p", "into": 0 }))).await;
    // Point the spawner at c, now that its id is known.
    let point = move |tx: Tx| async move {
        let record = tx.task(tree.p).await?.unwrap();
        let input = Json::from(json!({ "name": "p", "into": tree.c.0 }));
        tx.set_task(TaskRecord { input, ..record })
    };
    env.root.commit(point, &context()).await.unwrap();
    board.open("a", Ending::Completed);
    board.open("b", Ending::Completed);
    settles(&env.harness, tree.b).await;
    board.open("p", Ending::Completed);
    eventually(|| async { status(&env.harness, tree.p).await != TaskStatus::Running }).await;
    assert_eq!(status(&env.harness, tree.p).await, TaskStatus::Completing);
    board.open("inside.p", Ending::Completed);
    assert_eq!(settles(&env.harness, tree.p).await, "completed");
    env.harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn keeps_a_chain_when_one_commit_ends_its_last_task_and_creates_new_work_below_it() {
    let board = Board::default();
    let env = open(Arc::new(ControlledStorage::default()), &board).await;
    // Live p -> ended b -> conversation c with background task `old`, which keeps the chain known without holding b.
    let (id, hold) = (env.root.id, env.hold.clone());
    let stage = move |tx: Tx| async move {
        let p = tx.create_task(hold.clone(), hold_input("p", false), TaskOptions::conversation(id)).await?;
        let b = tx.create_task(hold.clone(), hold_input("b", false), TaskOptions::owned_by(p)).await?;
        let c = tx.create_conversation(ConversationOwnership::Task { task_id: b }).await?;
        let background = TaskOptions { background: true, ..TaskOptions::conversation(c.id) };
        let old = tx.create_task(hold, hold_input("old", false), background).await?;
        Ok((p, b, c.id, old))
    };
    let (p, b, c, old) = env.root.commit(stage, &context()).await.unwrap();
    board.open("b", Ending::Completed);
    settles(&env.harness, b).await;
    // One commit: the old task ends, which leaves nothing live below b, and a replacement starts in c.
    let hold = env.hold.clone();
    let swap = move |tx: Tx| async move {
        let ended = tx.task(old).await?.unwrap();
        let created = tx.create_task(hold, hold_input("new", false), TaskOptions::conversation(c)).await?;
        tx.set_task(TaskRecord { state: completed(json!(null)).unwrap(), ..ended })?;
        Ok(created)
    };
    let replacement = env.harness.commit(swap, &context()).await.unwrap();
    // The chain stayed known through the publication, so p holds for the replacement.
    board.open("p", Ending::Completed);
    eventually(|| async { status(&env.harness, p).await == TaskStatus::Completing }).await;
    board.open("new", Ending::Completed);
    settles(&env.harness, replacement).await;
    assert_eq!(settles(&env.harness, p).await, "completed");
    env.harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn drops_what_a_rejected_commit_s_chain_load_read_without_a_later_publication() {
    let board = Board::default();
    let env = open(Arc::new(ControlledStorage::default()), &board).await;
    // Live p owns ended b, which owns conversation c: forgotten. The terminal commit of p stages work in c, which
    // loads the chain of c, and a child of p, which the Transaction rejects, as p is ending.
    let (id, hold) = (env.root.id, env.hold.clone());
    let rejected = env.rejected.clone();
    let stage = move |tx: Tx| async move {
        let input = Json::from(json!({ "into": 0 }));
        let p = tx.create_task(rejected, input, TaskOptions::conversation(id)).await?;
        let b = tx.create_task(hold, hold_input("b", false), TaskOptions::owned_by(p)).await?;
        let c = tx.create_conversation(ConversationOwnership::Task { task_id: b }).await?;
        Ok((p, b, c.id))
    };
    let (p, b, c) = env.root.commit(stage, &context()).await.unwrap();
    let point = move |tx: Tx| async move {
        let record = tx.task(p).await?.unwrap();
        tx.set_task(TaskRecord { input: Json::from(json!({ "into": c.0 })), ..record })
    };
    env.root.commit(point, &context()).await.unwrap();
    board.open("b", Ending::Completed);
    settles(&env.harness, b).await;
    let baseline = sizes(&env.harness);
    board.open("rejected", Ending::Completed);
    eventually(|| async { !env.rejections.lock().unwrap().is_empty() }).await;
    // The rejected commit published nothing; the sweep job still cleans up what its chain load read.
    eventually(|| async { sizes(&env.harness).drop_queue == 0 }).await;
    assert_eq!(sizes(&env.harness), baseline);
    env.harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn drops_what_a_read_loaded_after_reopen_without_a_later_publication() {
    let storage = Arc::new(ControlledStorage::default());
    let board = Board::default();
    let env = open(storage.clone(), &board).await;
    // A live task d in conversation c, owned by ended b: after reopen the chain of d is not loaded.
    let (id, hold) = (env.root.id, env.hold.clone());
    let stage = move |tx: Tx| async move {
        let b = tx.create_task(hold.clone(), hold_input("b", false), TaskOptions::conversation(id)).await?;
        let c = tx.create_conversation(ConversationOwnership::Task { task_id: b }).await?;
        let d = tx.create_task(hold, hold_input("d", false), TaskOptions::conversation(c.id)).await?;
        Ok((b, d))
    };
    let (b, d) = env.root.commit(stage, &context()).await.unwrap();
    board.open("b", Ending::Completed);
    eventually(|| async { status(&env.harness, b).await == TaskStatus::Completing }).await;
    env.harness.close(&context()).await.unwrap();
    storage.reopen();
    let env = open(storage, &board).await;
    env.harness.inspect(&context()).await.unwrap();
    // d is still live, so its chain stays; once d ends, everything below the root goes, published or not.
    assert_eq!(sizes(&env.harness).unloaded, 0);
    board.open("d", Ending::Completed);
    env.harness.wait_for_task(d, &context()).await.unwrap();
    env.harness.wait_for_task(b, &context()).await.unwrap();
    eventually(|| async { sizes(&env.harness).drop_queue == 0 }).await;
    let after = sizes(&env.harness);
    assert_eq!((after.settled, after.below, after.unloaded), (0, 0, 0));
    env.harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn withdraws_an_input_queued_two_ended_levels_below_an_owner_whose_intent_appears_later() {
    let board = Board::default();
    let env = open(Arc::new(ControlledStorage::default()), &board).await;
    // Live p -> ended b -> ended a -> conversation c with no task, and a live child keeping p holding.
    let (id, hold) = (env.root.id, env.hold.clone());
    let stage = move |tx: Tx| async move {
        let p = tx.create_task(hold.clone(), hold_input("p", false), TaskOptions::conversation(id)).await?;
        let b = tx.create_task(hold.clone(), hold_input("b", false), TaskOptions::owned_by(p)).await?;
        let a = tx.create_task(hold.clone(), hold_input("a", false), TaskOptions::owned_by(b)).await?;
        let c = tx.create_conversation(ConversationOwnership::Task { task_id: a }).await?;
        let keep = tx.create_task(hold, hold_input("keep", false), TaskOptions::owned_by(p)).await?;
        Ok((p, b, c.id, keep))
    };
    let (p, b, c, keep) = env.root.commit(stage, &context()).await.unwrap();
    board.open("a", Ending::Completed);
    board.open("b", Ending::Completed);
    settles(&env.harness, b).await;
    let c = child(&env.harness, c).await;
    busy(&c, keep).await;
    // Queued while nothing cancels: no withdrawal scan.
    let queued = input(&c, "later").await;
    eventually(|| async { sizes(&env.harness).drop_queue == 0 }).await;
    // The task p fails while its child runs: it holds the failed outcome, which is cancellation intent.
    let fail = move |tx: Tx| async move {
        let record = tx.task(p).await?.unwrap();
        let outcome = TaskOutcome::Failed { error: TaskOutcomeError::new("x"), result: None };
        tx.set_task(TaskRecord { state: TaskState::Completing { outcome }, ..record })
    };
    env.harness.commit(fail, &context()).await.unwrap();
    eventually(|| async { submission(&env.harness, queued).await.status == SubmissionStatus::Unanswered }).await;
    assert_eq!(submission(&env.harness, queued).await.reason.as_deref(), Some("aborted"));
    env.harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn forgets_a_20_000_deep_chain_of_ended_tasks_without_recursion() {
    let board = Board::default();
    let env = open(Arc::new(ControlledStorage::default()), &board).await;
    let depth = 20_000;
    let baseline = sizes(&env.harness);
    let (id, hold) = (env.root.id, env.hold.clone());
    let stage = move |tx: Tx| async move {
        let first = tx.create_task(hold.clone(), hold_input("link0", false), TaskOptions::conversation(id)).await?;
        let (mut links, mut owner) = (vec![first], first);
        for n in 1..depth {
            owner = tx
                .create_task(hold.clone(), hold_input(&format!("link{n}"), false), TaskOptions::owned_by(owner))
                .await?;
            links.push(owner);
        }
        let leaf = tx.create_task(hold, hold_input("leaf", false), TaskOptions::owned_by(owner)).await?;
        Ok((links, leaf))
    };
    let (links, leaf) = env.root.commit(stage, &context()).await.unwrap();
    for n in 0..depth {
        board.open(&format!("link{n}"), Ending::Completed);
    }
    board.open("leaf", Ending::Completed);
    env.harness.wait_for_task(leaf, &context()).await.unwrap();
    env.harness.wait_for_task(links[0], &context()).await.unwrap();
    env.harness.wait_for_idle(&context()).await.unwrap();
    assert_eq!(sizes(&env.harness), baseline);
    env.harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn keeps_no_more_after_all_work_ended_than_a_fresh_reopen() {
    let storage = Arc::new(ControlledStorage::default());
    let board = Board::default();
    let env = open(storage.clone(), &board).await;
    let parent = create_hold(&env.root, &env.hold, "parent").await;
    for n in 0..20 {
        super::retention::subagent_cycle(&env, &board, parent, n).await;
    }
    board.open("parent", Ending::Completed);
    env.harness.wait_for_task(parent, &context()).await.unwrap();
    env.harness.wait_for_idle(&context()).await.unwrap();
    let ended = sizes(&env.harness);
    env.harness.close(&context()).await.unwrap();
    storage.reopen();
    let env = open(storage, &board).await;
    env.harness.wait_for_idle(&context()).await.unwrap();
    assert_eq!(ended, sizes(&env.harness));
    env.harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn does_not_scan_queued_submissions_for_an_input_when_nothing_cancels() {
    let storage = Arc::new(ControlledStorage::default());
    let scans = Arc::new(AtomicUsize::new(0));
    let counter = scans.clone();
    *storage.call_hook.lock().unwrap() = Some(Arc::new(move |call: StorageCall| {
        if call.method == "scan_queued_submissions" {
            counter.fetch_add(1, Ordering::SeqCst);
        }
        Box::pin(async { Ok(()) })
    }));
    let board = Board::default();
    let env = open(storage, &board).await;
    let busy_task = create_hold(&env.root, &env.hold, "busy").await;
    busy(&env.root, busy_task).await;
    let before = scans.load(Ordering::SeqCst);
    input(&env.root, "queued").await;
    // Let any reconcile it would cause run.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert_eq!(scans.load(Ordering::SeqCst), before);
    board.open("busy", Ending::Completed);
    env.harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn still_rejects_waiting_on_a_forgotten_ended_owner() {
    let board = Board::default();
    let Env { harness, root, hold, .. } = open(Arc::new(ControlledStorage::default()), &board).await;
    let (id, held) = (root.id, hold.clone());
    let stage = move |tx: Tx| async move {
        let b = tx.create_task(held, hold_input("b", false), TaskOptions::conversation(id)).await?;
        Ok((b, tx.create_conversation(ConversationOwnership::Task { task_id: b }).await?.id))
    };
    let (b, c) = root.commit(stage, &context()).await.unwrap();
    board.open("b", Ending::Completed);
    settles(&harness, b).await;
    let conversation = child(&harness, c).await;
    let on = Json::from(json!({ "on": [b] }));
    let create = move |tx: Tx| async move {
        tx.create_task(super::waiter_task(), on, TaskOptions::conversation(conversation.id)).await
    };
    let waiter = child(&harness, c).await.commit(create, &context()).await.unwrap();
    let settled = harness.wait_for_task(waiter, &context()).await.unwrap();
    let outcome = serde_json::to_value(settled.state.outcome()).unwrap();
    assert_eq!(outcome["status"], "faulted");
    assert!(outcome.to_string().contains("cannot wait on itself or its owner"), "{outcome}");
    harness.close(&context()).await.unwrap();
}
