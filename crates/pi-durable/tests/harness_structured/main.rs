//! Port of upstream `test/harness-structured.test.ts`. `nodes.rs` holds the scriptable `test.node` task and the
//! helpers; this file holds the task ownership cases, and the sibling modules the other upstream sections.

#[path = "../support/mod.rs"]
mod support;

mod completing;
mod definitions;
mod edges;
mod nodes;
mod recovery;
mod waiting;

use std::sync::{Arc, Mutex};

use nodes::*;
use pi_durable::harness::*;
use pi_durable::session::Tx;
use pi_durable::tasks::TaskOptions;
use pi_durable::*;
use support::context;
use support::task_support::eventually;

#[tokio::test]
async fn creates_a_child_in_its_owner_s_conversation_with_its_owner_recorded() {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    let child = Ids::default();
    let (sink, spawner) = (child.clone(), nodes.clone());
    nodes.run("parent", move |runtime, context| {
        let (sink, nodes) = (sink.clone(), spawner.clone());
        async move {
            let owner = runtime.task_id();
            let spawn_child = move |tx: Tx, _| async move {
                let child = spawn(&tx, &nodes, owner, "child").await?;
                sink.lock().unwrap().push(child);
                Ok(wait_on(vec![child], JoinPolicy::AllSettled, 1))
            };
            runtime.commit(spawn_child, &context).await
        }
    });
    let parent = start(&root, &nodes, "parent").await;
    eventually(|| async { !ids(&child).is_empty() }).await;
    let record = harness.get_task(ids(&child)[0], &context()).await.unwrap().unwrap();
    assert_eq!((record.owner, record.conversation_id, record.background), (Some(parent), root.id, false));
    assert_eq!(harness.get_task(parent, &context()).await.unwrap().unwrap().owner, None);
    nodes.open_ok("child");
    assert_eq!(outcome_of(&harness, parent).await, "completed");
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn rejects_no_ownership_a_missing_owner_a_child_in_another_conversation_and_a_background_child() {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    let parent = start(&root, &nodes, "parent").await;
    let ownerless = CreateOptions { ownership: ConversationOwnership::Ownerless, ..CreateOptions::default() };
    let other = harness.create_conversation(ownerless, &context()).await.unwrap();
    let create = |options: TaskOptions| {
        let (task, root) = (nodes.task.clone(), root.clone());
        async move {
            let create = move |tx: Tx| async move { tx.create_task(task, named("x"), options).await };
            root.commit(create, &context()).await.unwrap_err().to_string()
        }
    };
    // Rust `TaskOptions` always carries an ownership, so the upstream "no ownership" case cannot be written.
    let missing = create(TaskOptions::owned_by(TaskId::from(999_999))).await;
    assert!(missing.contains("does not exist"), "{missing}");
    let elsewhere = create(TaskOptions { conversation_id: Some(other.id), ..TaskOptions::owned_by(parent) }).await;
    assert!(elsewhere.contains("owner's conversation"), "{elsewhere}");
    let background = create(TaskOptions { background: true, ..TaskOptions::owned_by(parent) }).await;
    assert!(background.contains("cannot be background"), "{background}");
    // The owner's conversation is the default, even from a commit bound to another conversation.
    let spawner = nodes.clone();
    let spawn_child = move |tx: Tx| async move { spawn(&tx, &spawner, parent, "child").await };
    let child = other.commit(spawn_child, &context()).await.unwrap();
    assert_eq!(harness.get_task(child, &context()).await.unwrap().unwrap().conversation_id, root.id);
    nodes.open_ok("parent");
    nodes.open_ok("child");
    harness.wait_for_task(parent, &context()).await.unwrap();
    harness.close(&context()).await.unwrap();
}

async fn owned_work_error(root: &Conversation, nodes: &Nodes, owner: TaskId, conversation: bool) -> String {
    let nodes = nodes.clone();
    let create = move |tx: Tx| async move {
        if conversation {
            tx.create_conversation(ConversationOwnership::Task { task_id: owner }).await.map(|_| ())
        } else {
            spawn(&tx, &nodes, owner, "late").await.map(|_| ())
        }
    };
    root.commit(create, &context()).await.unwrap_err().to_string()
}

#[tokio::test]
async fn rejects_new_owned_work_below_an_owner_that_is_completing_terminal_or_abort_marked() {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    let spawner = nodes.clone();
    nodes.run("parent", move |runtime, context| {
        let nodes = spawner.clone();
        async move {
            let owner = runtime.task_id();
            let change = move |tx: Tx, _| async move {
                spawn(&tx, &nodes, owner, "child").await?;
                Ok(resume_at(1))
            };
            runtime.commit(change, &context).await
        }
    });
    let parent = start(&root, &nodes, "parent").await;
    until_status(&harness, parent, TaskStatus::Completing).await;
    for conversation in [false, true] {
        let error = owned_work_error(&root, &nodes, parent, conversation).await;
        assert!(error.contains("is completing"), "{error}");
    }
    nodes.open_ok("child");
    harness.wait_for_task(parent, &context()).await.unwrap();
    for conversation in [false, true] {
        let error = owned_work_error(&root, &nodes, parent, conversation).await;
        assert!(error.contains("is terminal"), "{error}");
    }

    nodes.slow_abort("slow");
    let slow = start(&root, &nodes, "slow").await;
    eventually(|| async { nodes.logged("run:slow") }).await;
    harness.abort_task(slow, &context()).await.unwrap();
    let error = owned_work_error(&root, &nodes, slow, false).await;
    assert!(error.contains("is abort-marked"), "{error}");
    nodes.open_ok("abort.slow");
    assert_eq!(outcome_of(&harness, slow).await, "aborted");
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn cannot_create_a_child_in_its_finishing_commit_but_work_it_starts_in_an_owned_conversation_holds_it() {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    let spawner = nodes.clone();
    nodes.run("eager", move |runtime, context| {
        let nodes = spawner.clone();
        async move {
            let owner = runtime.task_id();
            let change = move |tx: Tx, _| async move {
                spawn(&tx, &nodes, owner, "never").await?;
                Ok(end(Ending::Completed, "eager"))
            };
            runtime.commit(change, &context).await
        }
    });
    let eager = start(&root, &nodes, "eager").await;
    assert_faulted(&harness, eager, "is completing").await;
    // The rejected commit wrote nothing.
    let query = TaskQuery { kind: Some("test.node".into()), ..TaskQuery::default() };
    let page = harness.commit(move |tx| async move { tx.scan_tasks(query, 20, None).await }, &context());
    let names: Vec<String> = page.await.unwrap().items.iter().map(name_of).collect();
    assert_eq!(names, ["eager"]);

    let child = Arc::new(Mutex::new(None));
    let sink = child.clone();
    nodes.run("host", move |runtime, context| {
        let sink = sink.clone();
        async move {
            let owner = runtime.task_id();
            let change = move |tx: Tx, _| async move {
                let created = tx.create_conversation(ConversationOwnership::Task { task_id: owner }).await?;
                *sink.lock().unwrap() = Some(created.id);
                Ok(resume_at(1))
            };
            runtime.commit(change, &context).await
        }
    });
    let (source, spawner) = (child.clone(), nodes.clone());
    nodes.resume("host", move |runtime, context, _| {
        let (child, task) = (source.lock().unwrap().unwrap(), spawner.task.clone());
        async move {
            let change = move |tx: Tx, _| async move {
                tx.create_task(task, named("inner"), TaskOptions::conversation(child)).await?;
                Ok(end(Ending::Completed, "host"))
            };
            runtime.commit(change, &context).await
        }
    });
    let host = start(&root, &nodes, "host").await;
    until_status(&harness, host, TaskStatus::Completing).await;
    assert!(!settled(wait_task(&harness, host)).await);
    nodes.open_ok("inner");
    assert_eq!(outcome_of(&harness, host).await, "completed");
    harness.close(&context()).await.unwrap();
}
