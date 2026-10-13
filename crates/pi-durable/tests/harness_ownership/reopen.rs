//! Ownership across reopen, marks a crash left unapplied, and failed cascade commits.

use std::sync::{Arc, Mutex};

use pi_chord::Json;
use pi_durable::session::{SessionEnd, SessionOptions, Tx, create_session};
use pi_durable::storage::sqlite::open_sqlite_storage;
use pi_durable::tasks::TaskOptions;
use pi_durable::*;
use serde_json::json;

use super::support::task_support::eventually;
use super::support::{ControlledStorage, context};
use super::{
    BACKGROUND, Board, ChildOptions, Ending, Opened, child, create_hold, hold_input, hold_task, open_harness,
    owned_child, settled, settles, status,
};

async fn sqlite(directory: &tempfile::TempDir) -> Arc<dyn Storage> {
    let path = directory.path().join("session.sqlite");
    Arc::new(open_sqlite_storage(&path.to_string_lossy()).await.unwrap())
}

#[tokio::test]
async fn decides_idle_after_reopen_from_owner_edges_it_has_to_load_first() {
    let directory = tempfile::tempdir().unwrap();
    let board = Board::default();
    let Opened { harness, root, hold, .. } = open_harness(sqlite(&directory).await, &board).await;
    let foreground = owned_child(&root, &hold, "fg", ChildOptions::default()).await;
    let deeper = owned_child(&child(&harness, foreground.child).await, &hold, "fg2", ChildOptions::default()).await;
    let background = owned_child(&root, &hold, "bg", BACKGROUND).await;
    board.open("fg", Ending::Completed);
    board.open("fg2", Ending::Completed);
    // Both owners hold their outcomes while the work below them runs.
    eventually(|| async { status(&harness, foreground.owner).await == TaskStatus::Completing }).await;
    eventually(|| async { status(&harness, deeper.owner).await == TaskStatus::Completing }).await;
    harness.close(&context()).await.unwrap();

    let Opened { harness, root, .. } = open_harness(sqlite(&directory).await, &board).await;
    // Two levels below completed foreground owners, the inner tasks keep the root busy.
    assert!(!settled(&harness, Some(&root)).await);
    assert!(!settled(&harness, None).await);
    board.open("fg.inner", Ending::Completed);
    board.open("fg2.inner", Ending::Completed);
    // The background subtree does not count.
    root.wait_for_idle(&context()).await.unwrap();
    harness.wait_for_idle(&context()).await.unwrap();
    assert_eq!(status(&harness, foreground.owner).await, TaskStatus::Terminal);
    assert_ne!(status(&harness, background.inner).await, TaskStatus::Terminal);
    harness.abort_task(background.owner, &context()).await.unwrap();
    harness.close(&context()).await.unwrap();
}

async fn derives_marks_a_crash_left_unapplied(abort_requested: bool, state: Json) {
    let directory = tempfile::tempdir().unwrap();
    let board = Board::default();
    let hold = hold_task(&board);
    // Without a Harness, nothing derives marks: a cancelled owner with a live task below it.
    let session = create_session(sqlite(&directory).await, SessionOptions::default());
    let stage = move |tx: Tx| async move {
        let root = tx.create_conversation(ConversationOwnership::Ownerless).await?;
        let owner = tx.create_task(hold.clone(), hold_input("gone", false), TaskOptions::conversation(root.id)).await?;
        let child = tx.create_conversation(ConversationOwnership::Task { task_id: owner }).await?;
        let inner = tx.create_task(hold, hold_input("orphan", false), TaskOptions::conversation(child.id)).await?;
        Ok((owner, inner))
    };
    let (owner, inner) = session.commit(stage, &context()).await.unwrap();
    let state: TaskState = serde_json::from_value(state.to_serde()).unwrap();
    let cancel = move |tx: Tx| async move {
        let record = tx.task(owner).await?.unwrap();
        tx.set_task(TaskRecord { abort_requested, state, ..record })
    };
    session.commit(cancel, &context()).await.unwrap();
    session.close(&context()).await.unwrap();

    let Opened { harness, .. } = open_harness(sqlite(&directory).await, &board).await;
    assert_eq!(settles(&harness, inner).await, "aborted");
    // The owner finishes only after the work below it.
    assert_eq!(settles(&harness, owner).await, if abort_requested { "aborted" } else { "failed" });
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn derives_marks_a_crash_left_unapplied_below_a_held_failed_owner_at_open() {
    let state = json!({ "status": "completing", "outcome": { "status": "failed", "error": { "message": "crash" } } });
    derives_marks_a_crash_left_unapplied(false, Json::from(state)).await;
}

#[tokio::test]
async fn derives_marks_a_crash_left_unapplied_below_an_abort_marked_owner_at_open() {
    let state = json!({ "status": "pending", "checkpoint": { "phase": "hold" } });
    derives_marks_a_crash_left_unapplied(true, Json::from(state)).await;
}

#[tokio::test]
async fn marks_work_admitted_after_reopen_below_a_cancelled_owner_whose_edge_was_not_loaded() {
    let directory = tempfile::tempdir().unwrap();
    let board = Board::default();
    let Opened { harness, root, hold, .. } = open_harness(sqlite(&directory).await, &board).await;
    let tree = owned_child(&root, &hold, "owner", ChildOptions { slow_owner: true, ..Default::default() }).await;
    board.open("owner.inner", Ending::Completed);
    harness.wait_for_task(tree.inner, &context()).await.unwrap();
    // The owner stays live and cancelled in its slow abort handler.
    harness.abort_task(tree.owner, &context()).await.unwrap();
    harness.close(&context()).await.unwrap();

    // The child is empty at open, so nothing loads its edge until new work arrives.
    let Opened { harness, hold, .. } = open_harness(sqlite(&directory).await, &board).await;
    let late = create_hold(&child(&harness, tree.child).await, &hold, "late").await;
    assert_eq!(settles(&harness, late).await, "aborted");
    board.open("abort.owner", Ending::Completed);
    assert_eq!(settles(&harness, tree.owner).await, "aborted");
    harness.close(&context()).await.unwrap();
}

/// Fail the first commit that writes an abort-marked record of the task in `target`.
fn fail_mark_of(storage: &ControlledStorage, target: Arc<Mutex<Option<TaskId>>>, error: DurableError) {
    let filter = move |writes: &[StorageWrite]| {
        let id = *target.lock().unwrap();
        writes
            .iter()
            .any(|write| matches!(write, StorageWrite::Task { value } if Some(value.id) == id && value.abort_requested))
    };
    storage.fail_commit_where(filter, error);
}

// ControlledStorage stands in for the SQLite file here: its `reopen()` keeps the data like a reopened database.
#[tokio::test]
async fn applies_marks_found_through_an_edge_loaded_after_reopen_once_a_failed_cascade_commit_is_reopened() {
    let storage = Arc::new(ControlledStorage::default());
    let board = Board::default();
    let Opened { harness, root, hold, .. } = open_harness(storage.clone(), &board).await;
    let tree = owned_child(&root, &hold, "owner", ChildOptions { slow_owner: true, ..Default::default() }).await;
    board.open("owner.inner", Ending::Completed);
    harness.wait_for_task(tree.inner, &context()).await.unwrap();
    harness.abort_task(tree.owner, &context()).await.unwrap();
    harness.close(&context()).await.unwrap();

    let reject = Arc::new(Mutex::new(None));
    fail_mark_of(&storage, reject.clone(), DurableError::error("disk gone"));
    storage.reopen();
    let Opened { harness, hold, .. } = open_harness(storage.clone(), &board).await;
    let child = child(&harness, tree.child).await;
    let (id, target) = (child.id, reject.clone());
    let create = move |tx: Tx| async move {
        let created = tx.create_task(hold, hold_input("late", false), TaskOptions::conversation(id)).await?;
        *target.lock().unwrap() = Some(created);
        Ok(created)
    };
    let late = child.commit(create, &context()).await.unwrap();
    // The failed cascade commit fails the Harness. `closed` settles once its invocations have ended, and the owner's
    // abort handler ignores its signal: let it return.
    board.open("abort.owner", Ending::Completed);
    assert!(matches!(harness.closed().await, SessionEnd::Failed(_)));
    harness.close(&context()).await.unwrap();
    // Reopening derives the mark again.
    storage.reopen();
    let Opened { harness, .. } = open_harness(storage.clone(), &board).await;
    assert_eq!(settles(&harness, late).await, "aborted");
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn fails_the_harness_on_a_failed_cascade_commit_and_reopening_applies_the_cascade() {
    let storage = Arc::new(ControlledStorage::default());
    let board = Board::default();
    let Opened { harness, root, hold, reports } = open_harness(storage.clone(), &board).await;
    let tree = owned_child(&root, &hold, "owner", ChildOptions::default()).await;
    fail_mark_of(&storage, Arc::new(Mutex::new(Some(tree.inner))), DurableError::error("disk gone"));
    board.open("owner", Ending::Failed);
    let end = harness.closed().await;
    assert!(matches!(&end, SessionEnd::Failed(error) if error.to_string() == "disk gone"), "{end:?}");
    let reports: Vec<String> = reports.lock().unwrap().iter().map(ToString::to_string).collect();
    assert_eq!(reports, ["disk gone"]);
    harness.close(&context()).await.unwrap();
    storage.reopen();
    let Opened { harness, .. } = open_harness(storage.clone(), &board).await;
    assert_eq!(settles(&harness, tree.inner).await, "aborted");
    harness.close(&context()).await.unwrap();
}
