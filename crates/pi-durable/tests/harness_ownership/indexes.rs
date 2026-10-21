//! The scheduler's in-memory indexes: cases where a wrong index changes behavior, each against what the committed
//! records imply (spec 5.4, 5.5).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use pi_durable::harness::*;
use pi_durable::session::{SessionOptions, Tx, create_session};
use pi_durable::tasks::TaskOptions;
use pi_durable::*;

use super::support::task_support::{aborted, eventually, open_tasks};
use super::support::{ControlledStorage, Deferred, StorageCall, context};
use super::{
    Board, Ending, Opened, aborted_state, checkpoint, child, create_hold, hold_input, hold_task, open_harness, settled,
    settles, status,
};

/// The commit count, by order, of each write of a task for which `keep` holds.
fn task_writes(storage: &ControlledStorage, keep: impl Fn(&TaskRecord) -> bool) -> Vec<(usize, TaskRecord)> {
    let mut found = Vec::new();
    for (index, writes) in storage.commits().iter().enumerate() {
        for write in writes {
            if let StorageWrite::Task { value } = write
                && keep(value)
            {
                found.push((index, value.clone()));
            }
        }
    }
    found
}

#[tokio::test]
async fn takes_a_cascade_s_reason_from_the_nearest_cancelling_owner() {
    // Top (abort requested) -> middle (restart-marked) -> leaf: the leaf gets the restart of middle first, as the
    // nearest cancelling owner, and the request only once middle carries it.
    let storage = Arc::new(ControlledStorage::default());
    let board = Board::default();
    let hold = hold_task(&board);
    let session = create_session(storage.clone(), SessionOptions::default());
    let held = hold.clone();
    let stage = move |tx: Tx| async move {
        let root = tx.create_conversation(ConversationOwnership::Ownerless).await?;
        let top = tx.create_task(held.clone(), hold_input("top", false), TaskOptions::conversation(root.id)).await?;
        let middle = tx.create_task(held.clone(), hold_input("middle", false), TaskOptions::owned_by(top)).await?;
        let leaf = tx.create_task(held, hold_input("leaf", false), TaskOptions::owned_by(middle)).await?;
        Ok((top, middle, leaf))
    };
    let (top, middle, leaf) = session.commit(stage, &context()).await.unwrap();
    let mark = move |tx: Tx| async move {
        let (outer, inner) = (tx.task(top).await?.unwrap(), tx.task(middle).await?.unwrap());
        tx.set_task(TaskRecord { abort_requested: true, ..outer })?;
        tx.set_task(TaskRecord { abort_requested: true, abort_reason: Some(AbortReason::Restart), ..inner })
    };
    session.commit(mark, &context()).await.unwrap();
    session.close(&context()).await.unwrap();
    storage.reopen();
    let before = storage.commit_count();

    let opened = open_tasks(storage.clone(), vec![hold]).await;
    opened.harness.resume().unwrap();
    for id in [leaf, middle, top] {
        opened.harness.wait_for_task(id, &context()).await.unwrap();
    }
    let marks = task_writes(&storage, |record| record.id == leaf && record.abort_requested);
    assert!(marks.iter().all(|(index, _)| *index >= before));
    assert_eq!(marks[0].1.abort_reason, Some(AbortReason::Restart));
    opened.harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn finalizes_a_chain_of_held_owners_in_the_commit_that_frees_the_last_one() {
    let storage = Arc::new(ControlledStorage::default());
    let board = Board::default();
    let Opened { harness, root, hold, .. } = open_harness(storage.clone(), &board).await;
    let (id, held) = (root.id, hold.clone());
    let stage = move |tx: Tx| async move {
        let top = tx.create_task(held.clone(), hold_input("top", false), TaskOptions::conversation(id)).await?;
        let middle = tx.create_task(held.clone(), hold_input("middle", false), TaskOptions::owned_by(top)).await?;
        let leaf = tx.create_task(held, hold_input("leaf", false), TaskOptions::owned_by(middle)).await?;
        Ok((top, middle, leaf))
    };
    let (top, middle, leaf) = root.commit(stage, &context()).await.unwrap();
    board.open("top", Ending::Completed);
    board.open("middle", Ending::Completed);
    eventually(|| async { status(&harness, top).await == TaskStatus::Completing }).await;
    eventually(|| async { status(&harness, middle).await == TaskStatus::Completing }).await;
    board.open("leaf", Ending::Completed);
    assert_eq!(settles(&harness, top).await, "completed");
    let mut ended: HashMap<TaskId, usize> = HashMap::new();
    for (index, record) in task_writes(&storage, |record| record.state.status() == TaskStatus::Terminal) {
        ended.insert(record.id, index);
    }
    // Both owners end in one finalize commit, after the leaf's.
    assert_eq!(ended[&middle], ended[&top]);
    assert!(ended[&middle] > ended[&leaf]);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn keeps_a_scope_busy_after_reopen_for_work_below_an_ended_task_whose_chain_is_not_loaded_yet() {
    let storage = Arc::new(ControlledStorage::default());
    let board = Board::default();
    let Opened { harness, root, hold, .. } = open_harness(storage.clone(), &board).await;
    // Root conversation -> owner (ends) -> conversation c -> inner (created after owner ended), as a subagent call's.
    let (id, held) = (root.id, hold.clone());
    let stage = move |tx: Tx| async move {
        let owner = tx.create_task(held, hold_input("owner", false), TaskOptions::conversation(id)).await?;
        Ok((owner, tx.create_conversation(ConversationOwnership::Task { task_id: owner }).await?.id))
    };
    let (owner, owned) = root.commit(stage, &context()).await.unwrap();
    board.open("owner", Ending::Completed);
    assert_eq!(settles(&harness, owner).await, "completed");
    let inner = create_hold(&child(&harness, owned).await, &hold, "inner").await;
    harness.close(&context()).await.unwrap();

    // Hold the read of the record of c, so the chain above `inner` stays unloaded while idle is decided.
    storage.reopen();
    let held = Deferred::default();
    let gate = held.clone();
    let hook = move |call: StorageCall| {
        let gate = gate.clone();
        let wait = call.method == "conversation" && call.id == Some(owned.0);
        Box::pin(async move {
            if wait {
                gate.wait().await;
            }
            Ok(())
        }) as futures::future::BoxFuture<'static, DurableResult<()>>
    };
    *storage.call_hook.lock().unwrap() = Some(Arc::new(hook));
    let opened = open_tasks(storage.clone(), vec![hold]).await;
    let harness = opened.harness;
    // A task whose chain is not loaded counts as inside every scope.
    assert!(!settled(&harness, None).await);
    held.resolve();
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    harness.resume().unwrap();
    board.open("inner", Ending::Completed);
    assert_eq!(settles(&harness, inner).await, "completed");
    root.wait_for_idle(&context()).await.unwrap();
    harness.close(&context()).await.unwrap();
}

type Looked = Arc<Mutex<Option<Vec<TaskId>>>>;
type Runtimes = Arc<Mutex<Option<TaskRuntime>>>;

/// Records its owned tasks once its `look` gate opens, then holds until it is aborted.
fn probe_task(looked: &Looked, runtimes: &Runtimes, look: &Deferred) -> Task {
    let (looked, runtimes, look) = (looked.clone(), runtimes.clone(), look.clone());
    define_task(
        TaskDefinition::new("test.index.probe", 1, |_| Ok(checkpoint("look")))
            .phase("look", move |_, runtime, context| {
                let (looked, runtimes, look) = (looked.clone(), runtimes.clone(), look.clone());
                async move {
                    *runtimes.lock().unwrap() = Some(runtime.clone());
                    tokio::select! {
                        () = look.wait() => {}
                        () = aborted(runtime.signal()) => return Ok(()),
                    }
                    let owned = runtime.owned_tasks(&context).await?;
                    *looked.lock().unwrap() = Some(owned.iter().map(|record| record.id).collect());
                    aborted(runtime.signal()).await;
                    Ok(())
                }
            })
            .abort(|_, runtime, context| async move {
                runtime.commit(|_, _| async { Ok(aborted_state()) }, &context).await
            }),
    )
}

#[tokio::test]
async fn lists_the_live_tasks_a_task_owns_directly_in_id_order_also_after_a_reopen() {
    let storage = Arc::new(ControlledStorage::default());
    let board = Board::default();
    let hold = hold_task(&board);
    let (looked, runtimes, look) = (Looked::default(), Runtimes::default(), Deferred::default());
    let probe = probe_task(&looked, &runtimes, &look);
    let opened = open_tasks(storage.clone(), vec![hold.clone(), probe.clone()]).await;
    let harness = opened.harness;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    harness.resume().unwrap();
    // The probe owns: two held tasks, one that ends, a grandchild below the first, and a conversation with a task.
    let (id, held) = (root.id, hold.clone());
    let stage = move |tx: Tx| async move {
        let probe = tx.create_task(probe, hold_input("probe", false), TaskOptions::conversation(id)).await?;
        let first = tx.create_task(held.clone(), hold_input("first", false), TaskOptions::owned_by(probe)).await?;
        let ends = tx.create_task(held.clone(), hold_input("ends", false), TaskOptions::owned_by(probe)).await?;
        let second = tx.create_task(held.clone(), hold_input("second", false), TaskOptions::owned_by(probe)).await?;
        tx.create_task(held.clone(), hold_input("grandchild", false), TaskOptions::owned_by(first)).await?;
        let child = tx.create_conversation(ConversationOwnership::Task { task_id: probe }).await?;
        tx.create_task(held, hold_input("inside", false), TaskOptions::conversation(child.id)).await?;
        Ok((first, ends, second))
    };
    let (first, ends, second) = root.commit(stage, &context()).await.unwrap();
    board.open("ends", Ending::Completed);
    harness.wait_for_task(ends, &context()).await.unwrap();
    // The first ends too, but holds its outcome as completing while its grandchild runs: still live, so listed.
    board.open("first", Ending::Completed);
    eventually(|| async { status(&harness, first).await == TaskStatus::Completing }).await;
    look.resolve();
    eventually(|| async { looked.lock().unwrap().is_some() }).await;
    assert_eq!(looked.lock().unwrap().clone(), Some(vec![first, second]));
    harness.close(&context()).await.unwrap();

    // After a reopen the rerun sees the same tasks, which an earlier invocation created.
    storage.reopen();
    let (looked, runtimes, look) = (Looked::default(), Runtimes::default(), Deferred::default());
    let opened = open_tasks(storage.clone(), vec![hold, probe_task(&looked, &runtimes, &look)]).await;
    opened.harness.resume().unwrap();
    look.resolve();
    eventually(|| async { looked.lock().unwrap().is_some() }).await;
    assert_eq!(looked.lock().unwrap().clone(), Some(vec![first, second]));
    opened.harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn rejects_owned_tasks_once_its_invocation_has_ended() {
    let storage = Arc::new(ControlledStorage::default());
    let (looked, runtimes, look) = (Looked::default(), Runtimes::default(), Deferred::default());
    let probe = probe_task(&looked, &runtimes, &look);
    let opened = open_tasks(storage, vec![probe.clone()]).await;
    let harness = opened.harness;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    harness.resume().unwrap();
    let id = root.id;
    let create = move |tx: Tx| async move {
        tx.create_task(probe, hold_input("probe", false), TaskOptions::conversation(id)).await
    };
    let probe = root.commit(create, &context()).await.unwrap();
    eventually(|| async { runtimes.lock().unwrap().is_some() }).await;
    let runtime = runtimes.lock().unwrap().clone().unwrap();
    // Aborting joins the run invocation, which has ended by then.
    harness.abort_task(probe, &context()).await.unwrap();
    harness.wait_for_task(probe, &context()).await.unwrap();
    let error = runtime.owned_tasks(&context()).await.unwrap_err();
    assert!(error.to_string().contains("invocation has ended"), "{error}");
    harness.close(&context()).await.unwrap();
}
