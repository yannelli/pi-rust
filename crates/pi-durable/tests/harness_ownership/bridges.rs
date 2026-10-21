//! Ownership through ended tasks: work created later below them still belongs to the live owner above.

use std::time::Duration;

use pi_durable::harness::*;
use pi_durable::session::Tx;
use pi_durable::tasks::TaskOptions;
use pi_durable::*;
use serde_json::{Value, json};

use super::support::context;
use super::support::task_support::{completed, eventually};
use super::{
    Board, Ending, Opened, checkpoint, child, create_hold, hold_input, memory, open_harness, settled, settles, status,
};

/// Creates, in the commit that ends it, a conversation it owns with a held task in it.
pub fn spawner_task(hold: Task) -> Task {
    define_task(
        TaskDefinition::new("test.spawner", 1, |_| Ok(checkpoint("spawn")))
            .phase("spawn", move |task, runtime, context| {
                let hold = hold.clone();
                async move {
                    let name = task.input["name"].as_str().unwrap().to_owned();
                    let spawn = move |tx: Tx, record: TaskRecord| async move {
                        let owned = ConversationOwnership::Task { task_id: record.id };
                        let created = tx.create_conversation(owned).await?;
                        tx.create_task(hold, hold_input(&name, false), TaskOptions::conversation(created.id)).await?;
                        Ok(completed(Value::Null))
                    };
                    runtime.commit(spawn, &context).await
                }
            })
            .abort(|_, runtime, context| async move {
                runtime.commit(|_, _| async { Ok(super::aborted_state()) }, &context).await
            }),
    )
}

/// A task owned by `owner`, and the conversation the new task owns.
async fn owned_task(tx: &Tx, hold: &Task, name: &str, owner: TaskId) -> DurableResult<TaskId> {
    tx.create_task(hold.clone(), hold_input(name, false), TaskOptions::owned_by(owner)).await
}

async fn owned_conversation(tx: &Tx, owner: TaskId) -> DurableResult<ConversationId> {
    Ok(tx.create_conversation(ConversationOwnership::Task { task_id: owner }).await?.id)
}

#[tokio::test]
async fn holds_an_owner_for_work_in_a_conversation_its_ended_child_task_owns() {
    let board = Board::default();
    let Opened { harness, root, hold, .. } = memory(&board).await;
    let (id, held) = (root.id, hold.clone());
    let stage = move |tx: Tx| async move {
        let owner = tx.create_task(held.clone(), hold_input("owner", false), TaskOptions::conversation(id)).await?;
        let child = owned_task(&tx, &held, "child", owner).await?;
        Ok((owner, child, owned_conversation(&tx, child).await?))
    };
    let (owner, ended, conversation) = root.commit(stage, &context()).await.unwrap();
    board.open("child", Ending::Completed);
    assert_eq!(settles(&harness, ended).await, "completed");
    // The ended child's conversation still belongs to the owner above it.
    create_hold(&child(&harness, conversation).await, &hold, "late").await;
    board.open("owner", Ending::Completed);
    eventually(|| async { status(&harness, owner).await == TaskStatus::Completing }).await;
    board.open("late", Ending::Completed);
    assert_eq!(settles(&harness, owner).await, "completed");
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn cascades_through_ended_tasks_into_an_owned_background_subtree_when_aborting_across_background() {
    let board = Board::default();
    let Opened { harness, root, hold, .. } = memory(&board).await;
    // Root conversation -> a -> b -> conversation -> background c -> d.
    let (id, held) = (root.id, hold.clone());
    let stage = move |tx: Tx| async move {
        let a = tx.create_task(held.clone(), hold_input("a", false), TaskOptions::conversation(id)).await?;
        let b = owned_task(&tx, &held, "b", a).await?;
        let conversation = owned_conversation(&tx, b).await?;
        let owned = TaskOptions { background: true, ..TaskOptions::conversation(conversation) };
        let c = tx.create_task(held.clone(), hold_input("c", false), owned).await?;
        let d = owned_task(&tx, &held, "d", c).await?;
        Ok((a, c, d))
    };
    let (a, c, d) = root.commit(stage, &context()).await.unwrap();
    board.open("b", Ending::Completed);
    board.open("a", Ending::Completed);
    assert_eq!(settles(&harness, a).await, "completed");
    root.abort(&context(), true).await.unwrap();
    for id in [c, d] {
        assert_eq!(settles(&harness, id).await, "aborted");
    }
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn tracks_ownership_chains_deeper_than_the_call_stack() {
    let board = Board::default();
    let Opened { harness, root, hold, .. } = memory(&board).await;
    // Each task owns the next, created in one commit above a live leaf; they end top-down and hold.
    let depth = 20_000;
    let (id, held) = (root.id, hold.clone());
    let stage = move |tx: Tx| async move {
        let top = tx.create_task(held.clone(), hold_input("top", false), TaskOptions::conversation(id)).await?;
        let mut owner = top;
        for n in 0..depth {
            owner = owned_task(&tx, &held, &format!("link{n}"), owner).await?;
        }
        Ok((top, owned_task(&tx, &held, "leaf", owner).await?))
    };
    let (top, leaf) = root.commit(stage, &context()).await.unwrap();
    for n in 0..depth {
        board.open(&format!("link{n}"), Ending::Completed);
    }
    board.open("top", Ending::Completed);
    eventually_slow(|| async { status(&harness, top).await == TaskStatus::Completing }).await;
    board.open("leaf", Ending::Completed);
    assert_eq!(settles(&harness, leaf).await, "completed");
    assert_eq!(settles(&harness, top).await, "completed");
    harness.close(&context()).await.unwrap();
}

async fn eventually_slow<F, Fut>(mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    tokio::time::timeout(Duration::from_secs(60), async {
        while !check().await {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("Condition was not reached");
}

/// Live `p` owns `b`, which owns `a`, which owns the empty conversation `c`; `b` and `a` end, `c` stays.
struct Folded {
    p: TaskId,
    a: TaskId,
    b: TaskId,
    c: ConversationId,
}

async fn fold(root: &Conversation, hold: &Task, board: &Board) -> Folded {
    let (id, held) = (root.id, hold.clone());
    let stage = move |tx: Tx| async move {
        let p = tx.create_task(held.clone(), hold_input("p", false), TaskOptions::conversation(id)).await?;
        let b = owned_task(&tx, &held, "b", p).await?;
        let a = owned_task(&tx, &held, "a", b).await?;
        Ok(Folded { p, a, b, c: owned_conversation(&tx, a).await? })
    };
    let tree = root.commit(stage, &context()).await.unwrap();
    board.open("a", Ending::Completed);
    board.open("b", Ending::Completed);
    tree
}

async fn completes_after_late_work(harness: &Harness, hold: &Task, board: &Board, tree: &Folded) {
    let late = create_hold(&child(harness, tree.c).await, hold, "d").await;
    board.open("p", Ending::Completed);
    eventually(|| async { status(harness, tree.p).await == TaskStatus::Completing }).await;
    board.open("d", Ending::Completed);
    assert_eq!(settles(harness, late).await, "completed");
    assert_eq!(settles(harness, tree.p).await, "completed");
}

#[tokio::test]
async fn holds_an_owner_for_work_a_commit_creates_below_ended_tasks_loaded_from_storage() {
    let storage = std::sync::Arc::new(super::support::ControlledStorage::default());
    let board = Board::default();
    let Opened { harness, root, hold, .. } = open_harness(storage.clone(), &board).await;
    let tree = fold(&root, &hold, &board).await;
    harness.wait_for_task(tree.a, &context()).await.unwrap();
    harness.wait_for_task(tree.b, &context()).await.unwrap();
    harness.close(&context()).await.unwrap();
    // After reopen, `b` and `a` are known only once a chain load reads them.
    storage.reopen();
    let Opened { harness, hold, .. } = open_harness(storage.clone(), &board).await;
    completes_after_late_work(&harness, &hold, &board, &tree).await;
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn holds_an_owner_for_work_created_later_below_ended_tasks_without_a_reopen() {
    let board = Board::default();
    let Opened { harness, root, hold, .. } = memory(&board).await;
    let tree = fold(&root, &hold, &board).await;
    harness.wait_for_task(tree.b, &context()).await.unwrap();
    completes_after_late_work(&harness, &hold, &board, &tree).await;
    harness.close(&context()).await.unwrap();
}

/// Live `p` owns `depth` ended tasks in a chain, the last owning conversation `c`, empty when they ended. Work started
/// in `c` later is still `p`'s.
struct Bridge {
    harness: Harness,
    root: Conversation,
    hold: Task,
    board: Board,
    p: TaskId,
    c: Conversation,
}

impl Bridge {
    async fn open(depth: usize) -> Self {
        let board = Board::default();
        let Opened { harness, root, hold, .. } = memory(&board).await;
        let (id, held) = (root.id, hold.clone());
        let stage = move |tx: Tx| async move {
            let slow = hold_input("p", true);
            let p = tx.create_task(held.clone(), slow, TaskOptions::conversation(id)).await?;
            let (mut owner, mut links) = (p, Vec::new());
            for n in 0..depth {
                owner = owned_task(&tx, &held, &format!("link{n}"), owner).await?;
                links.push(owner);
            }
            Ok((p, links, owned_conversation(&tx, owner).await?))
        };
        let (p, links, c) = root.commit(stage, &context()).await.unwrap();
        for n in (0..depth).rev() {
            board.open(&format!("link{n}"), Ending::Completed);
        }
        for id in links {
            harness.wait_for_task(id, &context()).await.unwrap();
        }
        let c = child(&harness, c).await;
        Self { harness, root, hold, board, p, c }
    }

    async fn create(&self, name: &str) -> TaskId {
        create_hold(&self.c, &self.hold, name).await
    }
}

#[tokio::test]
async fn aborts_work_below_ended_tasks_with_their_live_owner_which_waits_for_it() {
    for depth in [2, 5] {
        let bridge = Bridge::open(depth).await;
        let d = bridge.create("d").await;
        bridge.harness.abort_task(bridge.p, &context()).await.unwrap();
        assert_eq!(settles(&bridge.harness, d).await, "aborted");
        // The abort handler of `p` ran only after `d` ended.
        bridge.board.open("abort.p", Ending::Completed);
        assert_eq!(settles(&bridge.harness, bridge.p).await, "aborted");
        bridge.harness.close(&context()).await.unwrap();
    }
}

#[tokio::test]
async fn keeps_the_root_busy_and_aborts_with_it_work_below_ended_tasks() {
    for depth in [2, 5] {
        let bridge = Bridge::open(depth).await;
        bridge.board.open("p", Ending::Completed);
        bridge.harness.wait_for_task(bridge.p, &context()).await.unwrap();
        // `p` has ended: `d` and `d2` hang below ended tasks only, and are still the root conversation's work.
        let (d, d2) = (bridge.create("d").await, bridge.create("d2").await);
        assert!(!settled(&bridge.harness, Some(&bridge.root)).await);
        assert!(!settled(&bridge.harness, None).await);
        bridge.root.abort(&context(), false).await.unwrap();
        for id in [d, d2] {
            assert_eq!(settles(&bridge.harness, id).await, "aborted");
        }
        bridge.harness.close(&context()).await.unwrap();
    }
}

#[tokio::test]
async fn reaches_work_below_ended_tasks_across_two_owned_conversations() {
    let board = Board::default();
    let Opened { harness, root, hold, .. } = memory(&board).await;
    // Live p -> ended b -> conversation c -> ended x -> ended a -> conversation c2; work starts in c2 later. The task
    // x owns no conversation, only the task a, so only owning a known task keeps it.
    let (id, held) = (root.id, hold.clone());
    let stage = move |tx: Tx| async move {
        let p = tx.create_task(held.clone(), hold_input("p", false), TaskOptions::conversation(id)).await?;
        let b = owned_task(&tx, &held, "b", p).await?;
        let c = owned_conversation(&tx, b).await?;
        let x = tx.create_task(held.clone(), hold_input("x", false), TaskOptions::conversation(c)).await?;
        let a = owned_task(&tx, &held, "a", x).await?;
        Ok((p, b, owned_conversation(&tx, a).await?))
    };
    let (p, b, c2) = root.commit(stage, &context()).await.unwrap();
    for name in ["a", "x", "b"] {
        board.open(name, Ending::Completed);
    }
    harness.wait_for_task(b, &context()).await.unwrap();
    let late = create_hold(&child(&harness, c2).await, &hold, "d").await;
    board.open("p", Ending::Completed);
    eventually(|| async { status(&harness, p).await == TaskStatus::Completing }).await;
    assert!(!settled(&harness, Some(&root)).await);
    root.abort(&context(), false).await.unwrap();
    assert_eq!(settles(&harness, late).await, "aborted");
    assert_eq!(settles(&harness, p).await, "completed");
    harness.close(&context()).await.unwrap();
}

async fn frees_after_a_background_task(aborted: bool) {
    let board = Board::default();
    let Opened { harness, root, hold, .. } = memory(&board).await;
    // Live p owns b, which owns a, which owns conversation c with background z and, later, foreground d.
    let (id, held) = (root.id, hold.clone());
    let stage = move |tx: Tx| async move {
        let p = tx.create_task(held.clone(), hold_input("p", false), TaskOptions::conversation(id)).await?;
        let b = owned_task(&tx, &held, "b", p).await?;
        let a = owned_task(&tx, &held, "a", b).await?;
        let c = owned_conversation(&tx, a).await?;
        let background = TaskOptions { background: true, ..TaskOptions::conversation(c) };
        let z = tx.create_task(held.clone(), hold_input("z", false), background).await?;
        Ok((p, b, c, z))
    };
    let (p, b, c, z) = root.commit(stage, &context()).await.unwrap();
    board.open("a", Ending::Completed);
    board.open("b", Ending::Completed);
    harness.wait_for_task(b, &context()).await.unwrap();
    let (conversation, held) = (child(&harness, c).await, hold.clone());
    let create = move |tx: Tx| async move {
        let slow = hold_input("d", true);
        tx.create_task(held, slow, TaskOptions::conversation(conversation.id)).await
    };
    let d = child(&harness, c).await.commit(create, &context()).await.unwrap();
    if aborted {
        // The task p waits for the abort of d before its own abort handler runs.
        harness.abort_task(p, &context()).await.unwrap();
        eventually(|| super::marked(&harness, d)).await;
    } else {
        board.open("p", Ending::Completed);
        eventually(|| async { status(&harness, p).await == TaskStatus::Completing }).await;
    }
    // The task z ends first, d last.
    board.open("z", Ending::Completed);
    harness.wait_for_task(z, &context()).await.unwrap();
    board.open(if aborted { "abort.d" } else { "d" }, Ending::Completed);
    harness.wait_for_task(d, &context()).await.unwrap();
    assert_eq!(settles(&harness, p).await, if aborted { "aborted" } else { "completed" });
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn frees_an_owner_that_completes_once_the_last_work_below_ended_tasks_ends_after_a_background_task() {
    frees_after_a_background_task(false).await;
}

#[tokio::test]
async fn frees_an_owner_that_is_aborted_once_the_last_work_below_ended_tasks_ends_after_a_background_task() {
    frees_after_a_background_task(true).await;
}

#[tokio::test]
async fn finalizes_many_held_siblings_that_end_together_in_one_pass() {
    let storage = std::sync::Arc::new(super::support::ControlledStorage::default());
    let board = Board::default();
    let Opened { harness, root, hold, .. } = open_harness(storage.clone(), &board).await;
    // Top holds 1000 held children, each holding one live leaf; the leaves end in one commit.
    let count = 1000;
    let (id, held) = (root.id, hold.clone());
    let stage = move |tx: Tx| async move {
        let top = tx.create_task(held.clone(), hold_input("top", false), TaskOptions::conversation(id)).await?;
        let mut siblings = Vec::new();
        for n in 0..count {
            siblings.push(owned_task(&tx, &held, &format!("s{n}"), top).await?);
        }
        // Leaves in reverse sibling order: a holder checked after each freed sibling would scan the finalized ones.
        let mut leaves = Vec::new();
        for n in (0..count).rev() {
            leaves.push(owned_task(&tx, &held, &format!("l{n}"), siblings[n]).await?);
        }
        Ok((top, leaves))
    };
    let (top, leaves) = root.commit(stage, &context()).await.unwrap();
    for n in 0..count {
        board.open(&format!("s{n}"), Ending::Completed);
    }
    board.open("top", Ending::Completed);
    eventually_slow(|| async { status(&harness, top).await == TaskStatus::Completing }).await;
    let before = storage.commit_count();
    let finish = move |tx: Tx| async move {
        let mut records = Vec::new();
        for id in leaves {
            records.push(tx.task(id).await?.unwrap());
        }
        for record in records {
            tx.set_task(TaskRecord { state: completed(json!(null)).unwrap(), ..record })?;
        }
        Ok(())
    };
    root.commit(finish, &context()).await.unwrap();
    assert_eq!(settles(&harness, top).await, "completed");
    // The leaves' commit, one round for every sibling, and one for the top.
    assert!(storage.commit_count() - before <= 4, "{} commits", storage.commit_count() - before);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn rejects_a_conversation_created_in_the_commit_that_ends_its_owner_task() {
    let board = Board::default();
    let Opened { harness, root, hold, .. } = memory(&board).await;
    let (id, spawner) = (root.id, spawner_task(hold));
    let create = move |tx: Tx| async move {
        tx.create_task(spawner, hold_input("spawned", false), TaskOptions::conversation(id)).await
    };
    let spawner = root.commit(create, &context()).await.unwrap();
    let settled = harness.wait_for_task(spawner, &context()).await.unwrap();
    let outcome = serde_json::to_value(settled.state.outcome()).unwrap();
    assert_eq!(outcome["status"], "faulted");
    let message = outcome["error"]["message"].as_str().unwrap();
    let pattern = format!("Conversation owner task {spawner} is ");
    assert!(
        message.starts_with(&pattern) && (message.ends_with("completing") || message.ends_with("terminal")),
        "{message}"
    );
    harness.close(&context()).await.unwrap();
}
