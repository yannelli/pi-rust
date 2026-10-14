//! Cascades from orphaned owners, cancelled waits and aborts, and marks that race a dependency.

use pi_chord::{AbortController, ChordError, Json, with_abort_signal};
use pi_durable::harness::*;
use pi_durable::session::Tx;
use pi_durable::tasks::TaskOptions;
use pi_durable::*;
use serde_json::{Value, json};

use super::support::context;
use super::support::task_support::{completed, eventually};
use super::{
    Board, ChildOptions, Ending, Opened, checkpoint, hold_input, memory, owned_child, settles, status, waiter_task,
};

#[tokio::test]
async fn cascades_from_an_owner_the_scheduler_orphans() {
    let board = Board::default();
    let Opened { harness, root, hold, .. } = memory(&board).await;
    // Never registered: aborting it can only orphan it.
    let unregistered = define_task(
        TaskDefinition::new("test.unregistered", 1, |_| Ok(checkpoint("hold")))
            .phase("hold", |_, _, _| async { Ok(()) })
            .abort(|_, _, _| async { Ok(()) }),
    );
    let id = root.id;
    let stage = move |tx: Tx| async move {
        let input = hold_input("unregistered", false);
        let owner = tx.create_task(unregistered, input, TaskOptions::conversation(id)).await?;
        let child = tx.create_conversation(ConversationOwnership::Task { task_id: owner }).await?;
        let inner = tx.create_task(hold, hold_input("below", false), TaskOptions::conversation(child.id)).await?;
        Ok((owner, inner))
    };
    let (owner, inner) = root.commit(stage, &context()).await.unwrap();
    assert_eq!(harness.abort_task(owner, &context()).await.unwrap(), AbortTaskResult::Marked);
    let settled = harness.wait_for_task(owner, &context()).await.unwrap();
    let outcome = serde_json::to_value(settled.state.outcome()).unwrap();
    assert_eq!(outcome, json!({ "status": "orphaned", "reason": "missing_task" }));
    assert_eq!(settles(&harness, inner).await, "aborted");
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn cancels_only_the_caller_s_wait_never_the_shared_work() {
    let board = Board::default();
    let Opened { harness, root, hold, .. } = memory(&board).await;
    let tree = owned_child(&root, &hold, "owner", ChildOptions::default()).await;
    let waiting = AbortController::new();
    let (waiter, wait_context) = (root.clone(), with_abort_signal(&waiting.signal(), &context()));
    let idle = tokio::spawn(async move { waiter.wait_for_idle(&wait_context).await });
    waiting.abort(Some(ChordError::error("stop waiting")));
    assert_eq!(idle.await.unwrap().unwrap_err().to_string(), "stop waiting");
    assert_ne!(status(&harness, tree.inner).await, TaskStatus::Terminal);

    // Cancelling an abort after its commit leaves the marks in place.
    let slow = owned_child(&root, &hold, "slow", ChildOptions::default()).await;
    let set_slow = move |tx: Tx| async move {
        let record = tx.task(slow.owner).await?.unwrap();
        tx.set_task(TaskRecord { input: hold_input("slow", true), ..record })
    };
    root.commit(set_slow, &context()).await.unwrap();
    let aborting = AbortController::new();
    let (aborter, abort_context) = (root.clone(), with_abort_signal(&aborting.signal(), &context()));
    let abort = tokio::spawn(async move { aborter.abort(&abort_context, false).await });
    eventually(|| async { harness.get_task(slow.owner, &context()).await.unwrap().unwrap().abort_requested }).await;
    aborting.abort(Some(ChordError::error("stop aborting")));
    assert_eq!(abort.await.unwrap().unwrap_err().to_string(), "stop aborting");
    board.open("abort.slow", Ending::Completed);
    for id in [tree.owner, tree.inner, slow.owner, slow.inner] {
        assert_eq!(settles(&harness, id).await, "aborted");
    }
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn aborts_a_waiting_child_whose_awaited_task_completes_in_the_commit_that_marks_its_owner() {
    let board = Board::default();
    let Opened { harness, root, hold, .. } = memory(&board).await;
    let id = root.id;
    let stage = move |tx: Tx| async move {
        let owner = tx.create_task(hold.clone(), hold_input("owner", false), TaskOptions::conversation(id)).await?;
        let child = tx.create_conversation(ConversationOwnership::Task { task_id: owner }).await?;
        let dependency = tx.create_task(hold, hold_input("dependency", false), TaskOptions::conversation(id)).await?;
        let input = Json::from(json!({ "on": [dependency] }));
        let blocked = tx.create_task(waiter_task(), input, TaskOptions::conversation(child.id)).await?;
        Ok((owner, dependency, blocked))
    };
    let (owner, dependency, blocked) = root.commit(stage, &context()).await.unwrap();
    eventually(|| async { board.runs("dependency") == 1 && board.runs("owner") == 1 }).await;
    eventually(|| async { status(&harness, blocked).await == TaskStatus::Waiting }).await;
    // One commit completes the dependency and marks the owner.
    let finish = move |tx: Tx| async move {
        let done = tx.task(dependency).await?.unwrap();
        let marked = tx.task(owner).await?.unwrap();
        tx.set_task(TaskRecord { state: completed(Value::Null).unwrap(), ..done })?;
        tx.set_task(TaskRecord { abort_requested: true, ..marked })
    };
    root.commit(finish, &context()).await.unwrap();
    assert_eq!(settles(&harness, blocked).await, "aborted");
    harness.close(&context()).await.unwrap();
}
