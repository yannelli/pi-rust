//! Port of upstream `test/harness-task-graph.test.ts`.

mod support;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::support::{ValueCall, on_value};
use pi_chord::delta::{Op, apply_immutable};
use pi_chord::state::{DeliveryKind, StateListener};
use pi_chord::{AbortController, ChordError, Json, with_abort_signal};
use pi_durable::harness::*;
use pi_durable::session::Tx;
use pi_durable::storage::MemoryStorage;
use pi_durable::storage::sqlite::open_sqlite_storage;
use pi_durable::tasks::TaskOptions;
use pi_durable::*;
use serde_json::{Value, json};
use support::task_support::{aborted, aborted_with, completed, eventually, open_tasks};
use support::{ControlledStorage, Deferred, context, flush};

fn ownership(task: TaskId) -> ConversationOwnership {
    ConversationOwnership::Task { task_id: task }
}

/// A parent that creates a child task and two conversations it owns, waits for the child, creates a second child,
/// and completes while that child is still live, so its outcome is held as `completing`.
fn family(child_gate: Deferred, late_gate: Deferred) -> (Task, Task) {
    let child = define_task(
        TaskDefinition::new("test.graph-child", 1, |_| Ok(Json::from(json!({ "phase": "work" }))))
            .phase("work", move |task, runtime, ctx| {
                let gate = if task.input.get("late").and_then(Json::as_bool) == Some(true) {
                    late_gate.clone()
                } else {
                    child_gate.clone()
                };
                async move {
                    gate.wait().await;
                    runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &ctx).await
                }
            })
            .abort(|_, _, _| async { Ok(()) }),
    );
    let spawned = child.clone();
    let late = child.clone();
    let parent = define_task(
        TaskDefinition::new("test.graph-parent", 1, |_| Ok(Json::from(json!({ "phase": "spawn" }))))
            .phase("spawn", move |task, runtime, ctx| {
                let child = spawned.clone();
                async move {
                    // Two conversations in a commit that leaves the parent's record unchanged.
                    let id = task.id;
                    let create = move |tx: Tx, _| async move {
                        tx.create_conversation(ownership(id)).await?;
                        tx.create_conversation(ownership(id)).await?;
                        Ok(None)
                    };
                    runtime.commit(create, &ctx).await?;
                    let wait = move |tx: Tx, _| async move {
                        let input = Json::from(json!({ "late": false }));
                        let child = tx.create_task(child, input, TaskOptions::owned_by(id)).await?;
                        let checkpoint = Json::from(json!({ "phase": "join", "child": child }));
                        Ok(Some(TaskState::Waiting { checkpoint, on: vec![child], policy: JoinPolicy::AllSettled }))
                    };
                    runtime.commit(wait, &ctx).await
                }
            })
            .phase("join", move |task, runtime, ctx| {
                let child = late.clone();
                async move {
                    let join = move |tx: Tx, _| async move {
                        let input = Json::from(json!({ "late": true }));
                        tx.create_task(child, input, TaskOptions::owned_by(task.id)).await?;
                        Ok(Some(TaskState::Running { checkpoint: Json::from(json!({ "phase": "finish" })) }))
                    };
                    runtime.commit(join, &ctx).await
                }
            })
            .phase("finish", |_, runtime, ctx| async move {
                runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &ctx).await
            })
            .abort(|_, _, _| async { Ok(()) }),
    );
    (parent, child)
}

fn value(json: &Json) -> Value {
    serde_json::to_value(json).unwrap()
}

/// Node statuses by `kind#id`, for compact assertions.
fn statuses(graph: &Json) -> BTreeMap<String, String> {
    let tasks = value(graph)["tasks"].as_object().cloned().unwrap_or_default();
    tasks
        .values()
        .map(|node| (format!("{}#{}", node["kind"].as_str().unwrap(), node["id"]), node["state"]["status"].to_string()))
        .map(|(key, status)| (key, status.trim_matches('"').to_owned()))
        .collect()
}

fn node(graph: &Json, id: impl ToString) -> Value {
    value(graph)["tasks"][id.to_string()].clone()
}

async fn conversation_task(root: &Conversation, task: &Task, input: Value, background: bool) -> TaskId {
    let (task, id) = (task.clone(), root.id);
    let create = move |tx: Tx| async move {
        let options = TaskOptions { background, ..TaskOptions::conversation(id) };
        tx.create_task(task, Json::from(input), options).await
    };
    root.commit(create, &context()).await.unwrap()
}

#[tokio::test]
async fn follows_every_live_task_through_its_statuses_owner_edges_and_owned_conversations() {
    let (child, late) = (Deferred::default(), Deferred::default());
    let (parent_task, child_task) = family(child.clone(), late.clone());
    let opened = open_tasks(Arc::new(MemoryStorage::new()), vec![parent_task.clone(), child_task]).await;
    let harness = opened.harness;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    let seen = Arc::new(Mutex::new(Vec::<Json>::new()));
    let observe = || async {
        let opened = harness.task_graph(&context()).await.unwrap();
        let sink = seen.clone();
        opened.subscribe(StateListener::sync(move |value, _, delivery| {
            if delivery.kind == DeliveryKind::Update {
                sink.lock().unwrap().push(value.clone());
            }
            Ok(())
        }));
        opened
    };
    let mut graph = observe().await;
    assert_eq!(value(&graph.value()), json!({ "tasks": {} }));

    let parent = conversation_task(&root, &parent_task, Value::Null, false).await;
    flush().await;
    assert_eq!(
        node(&graph.value(), parent),
        json!({
            "id": parent, "kind": "test.graph-parent", "conversationId": root.id, "background": false,
            "abortRequested": false, "state": { "status": "pending", "phase": "spawn" }, "conversations": [],
        })
    );

    harness.resume().unwrap();
    let child_id = |graph: &Json| -> Option<String> {
        let tasks = value(graph)["tasks"].as_object().cloned()?;
        tasks.values().find(|node| node["kind"] == "test.graph-child").map(|node| node["id"].to_string())
    };
    eventually(|| {
        let current = graph.value();
        let running = child_id(&current).is_some_and(|id| {
            statuses(&current).get(&format!("test.graph-child#{id}")).map(String::as_str) == Some("running")
        });
        async move { running && statuses(&current).len() == 2 }
    })
    .await;
    let first: u64 = child_id(&graph.value()).unwrap().parse().unwrap();
    let parent_node = node(&graph.value(), parent);
    assert_eq!(
        parent_node["state"],
        json!({ "status": "waiting", "phase": "join", "on": [first], "policy": "allSettled" })
    );
    let conversations: Vec<u64> = serde_json::from_value(parent_node["conversations"].clone()).unwrap();
    assert_eq!(conversations.len(), 2);
    assert!(conversations.is_sorted());
    assert_eq!(node(&graph.value(), first)["owner"], json!(parent));
    assert_eq!(node(&graph.value(), first)["conversationId"], json!(root.id));
    assert!(harness.conversation(ConversationId(conversations[0]), &context()).await.unwrap().is_some());

    // The advanced value equals a fresh build from Storage once the last observer left.
    let advanced = graph.value();
    graph.dispose().unwrap();
    graph = observe().await;
    assert!(!graph.value().same_ref(&advanced));
    assert_eq!(graph.value(), advanced);

    child.resolve();
    // The parent completes while the late child lives: its outcome is held.
    eventually(|| {
        let status = node(&graph.value(), parent)["state"]["status"].clone();
        async move { status == "completing" }
    })
    .await;
    assert_eq!(node(&graph.value(), parent)["state"], json!({ "status": "completing", "outcome": "completed" }));
    assert_eq!(node(&graph.value(), first), Value::Null);
    // Owned conversations stay listed while the owner lives.
    assert_eq!(node(&graph.value(), parent)["conversations"], parent_node["conversations"]);
    let advanced = graph.value();
    graph.dispose().unwrap();
    graph = observe().await;
    assert_eq!(graph.value(), advanced);

    late.resolve();
    harness.wait_for_task(parent, &context()).await.unwrap();
    flush().await;
    assert_eq!(value(&graph.value()), json!({ "tasks": {} }));
    let seen = seen.lock().unwrap().clone();
    // Every revision is one commit that changed a node; none repeats its predecessor.
    assert!(seen.windows(2).all(|pair| pair[0] != pair[1]));
    // The commit that created the two conversations published one revision setting them.
    assert!(seen.iter().any(|graph| node(graph, parent)["conversations"].as_array().is_some_and(|ids| ids.len() == 2)));
    graph.dispose().unwrap();
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn builds_from_committed_tasks_shows_surviving_tasks_as_pending_after_reopen_and_marks_aborts() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite").to_string_lossy().into_owned();
    let gate = Deferred::default();
    let work_gate = gate.clone();
    let work = define_task(
        TaskDefinition::new("test.graph-work", 1, |_| Ok(Json::from(json!({ "phase": "work" }))))
            .phase("work", move |_, runtime, ctx| {
                let gate = work_gate.clone();
                async move {
                    tokio::select! {
                        () = gate.wait() => {}
                        () = aborted(runtime.signal()) => return Err(DurableError::aborted()),
                    }
                    runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &ctx).await
                }
            })
            .abort(
                |_, runtime, ctx| async move { runtime.commit(|_, _| async { Ok(aborted_with("test")) }, &ctx).await },
            ),
    );
    let storage = Arc::new(open_sqlite_storage(&path).await.unwrap());
    let first = open_tasks(storage, vec![work.clone()]).await.harness;
    let first_root = first.root(&context(), CreateOptions::default()).await.unwrap();
    let (task, id) = (work.clone(), first_root.id);
    let create = move |tx: Tx| async move {
        let foreground = tx.create_task(task.clone(), Json::Null, TaskOptions::conversation(id)).await?;
        let options = TaskOptions { background: true, ..TaskOptions::conversation(id) };
        let background = tx.create_task(task, Json::Null, options).await?;
        let owned = tx.create_conversation(ownership(foreground)).await?;
        Ok((foreground, background, owned.id))
    };
    let (foreground, background, owned) = first_root.commit(create, &context()).await.unwrap();
    first.resume().unwrap();
    eventually(|| async {
        let inspection = first.inspect(&context()).await.unwrap();
        inspection.tasks.iter().all(|task| task.state == TaskInspectionState::Running)
    })
    .await;
    let running = first.task_graph(&context()).await.unwrap();
    let expected = |status: &str| {
        BTreeMap::from([
            (format!("test.graph-work#{foreground}"), status.to_owned()),
            (format!("test.graph-work#{background}"), status.to_owned()),
        ])
    };
    assert_eq!(statuses(&running.value()), expected("running"));
    first.close(&context()).await.unwrap();

    // Acquired after reopen: built from the committed records and owner edges; open reconciled running to pending.
    let storage = Arc::new(open_sqlite_storage(&path).await.unwrap());
    let harness = open_tasks(storage, vec![work]).await.harness;
    let watch = harness.watch_task_graph(&context()).await.unwrap();
    assert_eq!(statuses(&watch.value()), expected("pending"));
    assert_eq!(node(&watch.value(), foreground)["conversations"], json!([owned]));
    assert_eq!(node(&watch.value(), background)["background"], json!(true));

    // Exact frames: replaying their operations from the acquisition revision gives each delivered value.
    let replica = Arc::new(Mutex::new(watch.value()));
    let frames = Arc::new(Mutex::new(Vec::<Vec<Op>>::new()));
    let (sink, frame_sink) = (replica.clone(), frames.clone());
    watch
        .start(on_value(move |ValueCall { value, ops, .. }, _| {
            let next = apply_immutable(Some(&sink.lock().unwrap()), &ops).unwrap();
            assert_eq!(next, value);
            *sink.lock().unwrap() = next;
            frame_sink.lock().unwrap().push(ops);
            async { Ok(()) }
        }))
        .unwrap();
    harness.resume().unwrap();
    eventually(|| {
        let status = node(&watch.value(), background)["state"]["status"].clone();
        async move { status == "running" }
    })
    .await;
    assert_eq!(harness.abort_task(background, &context()).await.unwrap(), AbortTaskResult::Marked);
    harness.wait_for_task(background, &context()).await.unwrap();
    flush().await;
    let frames: Vec<Value> = frames.lock().unwrap().iter().map(|ops| serde_json::to_value(ops).unwrap()).collect();
    assert!(frames.iter().any(|ops| {
        ops.as_array().unwrap().len() == 1
            && ops[0][0] == "s"
            && ops[0][1] == json!(["tasks", background.to_string()])
            && ops[0][2]["abortRequested"] == true
    }));
    assert_eq!(frames.last().unwrap(), &json!([["d", ["tasks", background.to_string()]]]));
    let replica = value(&replica.lock().unwrap());
    assert_eq!(replica["tasks"].as_object().unwrap().keys().collect::<Vec<_>>(), [&foreground.to_string()]);
    watch.stop().await;
    gate.resolve();
    harness.wait_for_task(foreground, &context()).await.unwrap();
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn lists_owned_conversations_in_id_order_whatever_order_one_commit_creates_them_in() {
    let gate = Deferred::default();
    let created = Arc::new(Mutex::new(Vec::<u64>::new()));
    let (spawn_gate, sink) = (gate.clone(), created.clone());
    let spawner = define_task(
        TaskDefinition::new("test.graph-spawner", 1, |_| Ok(Json::from(json!({ "phase": "spawn" }))))
            .phase("spawn", move |task, runtime, ctx| {
                let (gate, sink) = (spawn_gate.clone(), sink.clone());
                async move {
                    let at = EntryId(task.input.get("at").and_then(Json::as_u64).unwrap());
                    let (conversation, id) = (runtime.conversation_id(), task.id);
                    let spawn = move |tx: Tx, _| async move {
                        let (forked, fresh) = futures::join!(
                            tx.fork_conversation(conversation, at, ownership(id)),
                            tx.create_conversation(ownership(id)),
                        );
                        sink.lock().unwrap().extend([forked?.id.0, fresh?.id.0]);
                        Ok(None)
                    };
                    runtime.commit(spawn, &ctx).await?;
                    gate.wait().await;
                    runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &ctx).await
                }
            })
            .abort(|_, _, _| async { Ok(()) }),
    );
    let harness = open_tasks(Arc::new(MemoryStorage::new()), vec![spawner.clone()]).await.harness;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    let graph = harness.task_graph(&context()).await.unwrap();
    let root_id = root.id;
    let create = move |tx: Tx| async move {
        let entry = tx.append_entry(root_id, EntryDraft::new("note")).await?;
        let input = Json::from(json!({ "at": entry.id }));
        tx.create_task(spawner, input, TaskOptions::conversation(root_id)).await
    };
    let id = root.commit(create, &context()).await.unwrap();
    harness.resume().unwrap();
    eventually(|| {
        let count = node(&graph.value(), id)["conversations"].as_array().map_or(0, Vec::len);
        async move { count == 2 }
    })
    .await;
    let advanced = graph.value();
    let mut sorted = created.lock().unwrap().clone();
    sorted.sort();
    assert_eq!(node(&advanced, id)["conversations"], json!(sorted));
    graph.dispose().unwrap();
    let rebuilt = harness.task_graph(&context()).await.unwrap();
    assert_eq!(rebuilt.value(), advanced);
    rebuilt.dispose().unwrap();
    gate.resolve();
    harness.wait_for_task(id, &context()).await.unwrap();
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn registers_nothing_for_an_acquisition_cancelled_while_it_waits_for_the_line() {
    let storage = Arc::new(ControlledStorage::default());
    let harness = open_tasks(storage.clone(), Vec::new()).await.harness;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    let (_, child) = family(Deferred::default(), Deferred::default());
    // The task's kind is not registered; it stays pending and still shows in the graph.
    conversation_task(&root, &child, json!({ "late": false }), false).await;
    let held = storage.hold_commits();
    let root_id = root.id;
    let session = harness.session().clone();
    let blocking = tokio::spawn(async move {
        session
            .commit(
                move |tx: Tx| async move { tx.append_entry(root_id, EntryDraft::new("blocker")).await.map(|_| ()) },
                &context(),
            )
            .await
    });
    held.entered().await;
    let controller = AbortController::new();
    let cancelled_context = with_abort_signal(&controller.signal(), &context());
    let watching = harness.clone();
    let cancelled = tokio::spawn(async move { watching.watch_task_graph(&cancelled_context).await.map(|_| ()) });
    flush().await;
    controller.abort(Some(ChordError::error("cancelled")));
    held.release();
    blocking.await.unwrap().unwrap();
    let error = cancelled.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("cancelled"), "{error}");
    // No observer kept the mount: each new observer builds a new revision.
    let first = harness.task_graph(&context()).await.unwrap();
    let value = first.value();
    first.dispose().unwrap();
    let second = harness.task_graph(&context()).await.unwrap();
    assert!(!second.value().same_ref(&value));
    assert_eq!(second.value(), value);
    second.dispose().unwrap();
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn publishes_no_revision_for_a_commit_that_changes_no_node_and_shares_one_mount_between_observers() {
    let (gate, reached) = (Deferred::default(), Deferred::default());
    let (work_gate, work_reached) = (gate.clone(), reached.clone());
    let memo = define_task(
        TaskDefinition::new("test.graph-memo", 1, |_| Ok(Json::from(json!({ "phase": "work" }))))
            .phase("work", move |_, runtime, ctx| {
                let (gate, reached) = (work_gate.clone(), work_reached.clone());
                async move {
                    runtime.memo_or("seen", Json::from(true), &ctx).await?;
                    reached.resolve();
                    gate.wait().await;
                    runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &ctx).await
                }
            })
            .abort(|_, _, _| async { Ok(()) }),
    );
    let harness = open_tasks(Arc::new(MemoryStorage::new()), vec![memo.clone()]).await.harness;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    let id = conversation_task(&root, &memo, Value::Null, false).await;
    let first = harness.task_graph(&context()).await.unwrap();
    let second = harness.task_graph(&context()).await.unwrap();
    assert!(second.value().same_ref(&first.value()));
    let updates = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = updates.clone();
    first.subscribe(StateListener::sync(move |value, _, delivery| {
        if delivery.kind == DeliveryKind::Update {
            let status = node(value, id)["state"]["status"].as_str().unwrap_or("gone").to_owned();
            sink.lock().unwrap().push(status);
        }
        Ok(())
    }));
    harness.resume().unwrap();
    reached.wait().await;
    flush().await;
    // Reservation changed the node; the memo commit did not.
    assert_eq!(*updates.lock().unwrap(), ["running"]);
    gate.resolve();
    harness.wait_for_task(id, &context()).await.unwrap();
    flush().await;
    assert_eq!(*updates.lock().unwrap(), ["running", "gone"]);
    let last = first.value();
    first.dispose().unwrap();
    second.dispose().unwrap();
    // No observer is left, so the mount was dropped: a new observer builds a new revision.
    let rebuilt = harness.task_graph(&context()).await.unwrap();
    assert_eq!(rebuilt.value(), last);
    assert!(!rebuilt.value().same_ref(&last));
    rebuilt.dispose().unwrap();
    harness.close(&context()).await.unwrap();
}
