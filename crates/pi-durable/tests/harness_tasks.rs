//! Port of upstream `test/harness-tasks.test.ts` (phases, scheduling, and abort).

mod support;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_chord::{AbortController, ChordError, Json, with_abort_signal};
use pi_durable::harness::*;
use pi_durable::session::Tx;
use pi_durable::storage::MemoryStorage;
use pi_durable::tasks::TaskOptions;
use pi_durable::*;
use serde_json::{Value, json};
use support::harness_support::chat_setup;
use support::{Deferred, context};

fn completed(result: Value) -> Option<TaskState> {
    Some(TaskState::Terminal { outcome: TaskOutcome::Completed { result: Some(Json::from(result)) } })
}

fn aborted_with(reason: &str) -> Option<TaskState> {
    Some(TaskState::Terminal { outcome: TaskOutcome::Aborted { reason: Some(reason.into()), result: None } })
}

fn running(checkpoint: Value) -> Option<TaskState> {
    Some(TaskState::Running { checkpoint: Json::from(checkpoint) })
}

/// A one-phase task whose abort handler settles `aborted` with reason "test".
fn one_step<F, Fut>(name: &str, run: F) -> Task
where
    F: Fn(TaskRecord, TaskRuntime, pi_chord::Context) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = DurableResult<()>> + Send + 'static,
{
    define_task(TaskDefinition::new(name, 1, |_| Ok(Json::from(json!({ "phase": "run" })))).phase("run", run).abort(
        |_, runtime, context| async move { runtime.commit(|_, _| async { Ok(aborted_with("test")) }, &context).await },
    ))
}

async fn open_root(tasks: Vec<Task>) -> (Harness, Conversation, Arc<Mutex<Vec<DurableError>>>) {
    let registry = Arc::new(create_registry());
    if !tasks.is_empty() {
        let extension = tasks.into_iter().fold(Extension::new("tasks"), Extension::task);
        registry.install(extension).unwrap();
    }
    let reports = Arc::new(Mutex::new(Vec::new()));
    let sink = reports.clone();
    let options = HarnessOptions::new(chat_setup(Default::default()).models, registry as Arc<dyn RegistryReader>)
        .with_on_report(move |error| sink.lock().unwrap().push(error.clone()));
    let harness = Harness::open(Arc::new(MemoryStorage::new()), options, &context()).await.unwrap();
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    (harness, root, reports)
}

async fn start(conversation: &Conversation, task: &Task, background: bool) -> TaskId {
    let (task, id) = (task.clone(), conversation.id);
    let create = move |tx: Tx| async move {
        let options = TaskOptions { background, ..TaskOptions::conversation(id) };
        tx.create_task(task, Json::Null, options).await
    };
    conversation.commit(create, &context()).await.unwrap()
}

async fn outcome(harness: &Harness, id: TaskId) -> Value {
    let record = harness.wait_for_task(id, &context()).await.unwrap();
    serde_json::to_value(record.state.outcome()).unwrap()
}

async fn eventually(mut check: impl FnMut() -> bool) {
    for _ in 0..400 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("Condition was not reached");
}

#[tokio::test]
async fn continues_one_invocation_through_checkpoint_progress_and_completes_with_a_typed_result() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let counter = define_task(
        TaskDefinition::new("test.counter", 1, |_| Ok(Json::from(json!({ "phase": "count", "n": 0 }))))
            .phase("count", move |task, runtime, context| {
                let sink = sink.clone();
                async move {
                    let n = task.state.checkpoint().and_then(|c| c.get("n")?.as_u64()).unwrap();
                    sink.lock().unwrap().push(n);
                    let to = task.input.get("to").and_then(Json::as_u64).unwrap();
                    let change = move |_, current: TaskRecord| async move {
                        let n = current.state.checkpoint().and_then(|c| c.get("n")?.as_u64()).unwrap() + 1;
                        Ok(if n == to { completed(json!(n)) } else { running(json!({ "phase": "count", "n": n })) })
                    };
                    runtime.commit(change, &context).await
                }
            })
            .abort(|_, _, _| async { Ok(()) }),
    );
    let (harness, root, _) = open_root(vec![counter.clone()]).await;
    let id = root.id;
    let create = move |tx: Tx| async move {
        tx.create_task(counter, Json::from(json!({ "to": 3 })), TaskOptions::conversation(id)).await
    };
    let task_id = root.commit(create, &context()).await.unwrap();
    harness.resume().unwrap();
    assert_eq!(outcome(&harness, task_id).await, json!({ "status": "completed", "result": 3 }));
    assert_eq!(*seen.lock().unwrap(), [0, 1, 2]);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn faults_a_phase_without_durable_progress_and_a_throwing_phase() {
    let idle = one_step("test.idle", |_, _, _| async { Ok(()) });
    let document_only = one_step("test.document-only", |_, runtime, context| async move {
        // A commit that returns no state is not progress.
        runtime.commit(|_, _| async { Ok(None) }, &context).await
    });
    let throws = one_step("test.throws", |_, _, _| async { Err(DurableError::error("boom")) });
    let (harness, root, _) = open_root(vec![idle.clone(), document_only.clone(), throws.clone()]).await;
    let ids = [
        start(&root, &idle, false).await,
        start(&root, &document_only, false).await,
        start(&root, &throws, false).await,
    ];
    harness.resume().unwrap();
    let mut outcomes = Vec::new();
    for id in ids {
        outcomes.push(outcome(&harness, id).await);
    }
    assert_eq!(
        outcomes,
        [
            json!({ "status": "faulted", "error": { "message": "Task test.idle phase run returned without durable progress" } }),
            json!({ "status": "faulted", "error": { "message": "Task test.document-only phase run returned without durable progress" } }),
            json!({ "status": "faulted", "error": { "message": "boom" } }),
        ]
    );
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn keeps_a_committed_terminal_outcome_when_the_handler_throws_afterwards() {
    let late = Arc::new(Mutex::new(None));
    let sink = late.clone();
    let done = one_step("test.done", move |_, runtime, context| {
        let sink = sink.clone();
        async move {
            runtime.commit(|_, _| async { Ok(completed(json!("ok"))) }, &context).await?;
            let error = runtime.commit(|_, _| async { Ok(None) }, &context).await.err();
            *sink.lock().unwrap() = error.map(|error| error.to_string());
            Err(DurableError::error("after terminal"))
        }
    });
    let (harness, root, _) = open_root(vec![done.clone()]).await;
    let id = start(&root, &done, false).await;
    harness.resume().unwrap();
    assert_eq!(outcome(&harness, id).await, json!({ "status": "completed", "result": "ok" }));
    eventually(|| late.lock().unwrap().is_some()).await;
    let late = late.lock().unwrap().clone().unwrap();
    assert!(late.contains(&format!("Task {id} is terminal")), "{late}");
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn compares_checkpoints_by_value_including_arrays() {
    let collect = define_task(
        TaskDefinition::new("test.collect", 1, |_| Ok(Json::from(json!({ "phase": "collect", "items": [] }))))
            .phase("collect", |task, runtime, context| async move {
                let items = task.state.checkpoint().and_then(|c| c.get("items").cloned()).unwrap().to_serde();
                let mut items = items.as_array().unwrap().clone();
                // Two rounds of progress, then an equal copy of the checkpoint, which is no progress.
                if items.len() < 2 {
                    items.push(json!(format!("item{}", items.len())));
                }
                let next = running(json!({ "phase": "collect", "items": items }));
                runtime.commit(move |_, _| async move { Ok(next) }, &context).await
            })
            .abort(|_, _, _| async { Ok(()) }),
    );
    let (harness, root, _) = open_root(vec![collect.clone()]).await;
    let id = start(&root, &collect, false).await;
    harness.resume().unwrap();
    assert_eq!(
        outcome(&harness, id).await,
        json!({ "status": "faulted", "error": { "message": "Task test.collect phase collect returned without durable progress" } })
    );
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn waits_for_harness_and_conversation_idleness_counting_blocked_work_and_ignoring_background_tasks() {
    let gates: Arc<Mutex<HashMap<TaskId, Deferred>>> = Arc::default();
    let sink = gates.clone();
    let gated = one_step("test.gated", move |task, runtime, context| {
        let gate = Deferred::default();
        sink.lock().unwrap().insert(task.id, gate.clone());
        async move {
            gate.wait().await;
            runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &context).await
        }
    });
    let (harness, root, _) = open_root(vec![gated.clone()]).await;
    harness.wait_for_idle(&context()).await.unwrap();
    let other = harness.create_conversation(CreateOptions::default(), &context()).await.unwrap();
    let foreground = start(&root, &gated, false).await;
    let background = start(&root, &gated, true).await;
    let elsewhere = start(&other, &gated, false).await;
    // Work that has not started yet is live; cancelling a wait only rejects that wait.
    let controller = AbortController::new();
    controller.abort(Some(ChordError::error("already cancelled")));
    let error = root.wait_for_idle(&with_abort_signal(&controller.signal(), &context())).await.err().unwrap();
    assert!(error.to_string().contains("already cancelled"), "{error}");
    harness.resume().unwrap();
    eventually(|| gates.lock().unwrap().len() == 3).await;
    let root_idle = {
        let root = root.clone();
        tokio::spawn(async move { root.wait_for_idle(&context()).await })
    };
    let harness_idle = {
        let harness = harness.clone();
        tokio::spawn(async move { harness.wait_for_idle(&context()).await })
    };
    gates.lock().unwrap()[&foreground].resolve();
    root_idle.await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!harness_idle.is_finished());
    gates.lock().unwrap()[&elsewhere].resolve();
    harness_idle.await.unwrap().unwrap();
    let record = harness.get_task(background, &context()).await.unwrap().unwrap();
    assert_eq!(record.state.status(), TaskStatus::Running);
    gates.lock().unwrap()[&background].resolve();
    harness.wait_for_task(background, &context()).await.unwrap();
    harness.close(&context()).await.unwrap();
    assert!(harness.wait_for_idle(&context()).await.err().unwrap().to_string().contains("closed"));
    assert!(root.wait_for_idle(&context()).await.err().unwrap().to_string().contains("closed"));
}

#[tokio::test]
async fn rejects_unknown_tasks_and_reports_terminal_tasks() {
    let done = one_step("test.quick", |_, runtime, context| async move {
        runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &context).await
    });
    let (harness, root, _) = open_root(vec![done.clone()]).await;
    let unknown = TaskId(999_999);
    assert!(harness.get_task(unknown, &context()).await.unwrap().is_none());
    assert!(harness.wait_for_task(unknown, &context()).await.err().unwrap().to_string().contains("does not exist"));
    assert!(harness.abort_task(unknown, &context()).await.err().unwrap().to_string().contains("does not exist"));
    let id = start(&root, &done, false).await;
    harness.resume().unwrap();
    harness.wait_for_task(id, &context()).await.unwrap();
    let again = harness.wait_for_task(id, &context()).await.unwrap();
    assert_eq!(again.state.status(), TaskStatus::Terminal);
    assert_eq!(harness.abort_task(id, &context()).await.unwrap(), AbortTaskResult::Terminal);
    harness.close(&context()).await.unwrap();
    assert!(harness.resume().err().unwrap().to_string().contains("closed"));
}

#[tokio::test]
async fn rejects_the_runs_commits_and_memo_writes_after_the_mark_and_settles_through_a_fresh_abort_invocation() {
    let reached = Deferred::default();
    let errors = Arc::new(Mutex::new(Vec::new()));
    let memo_read = Arc::new(Mutex::new(None));
    let (signal_reached, sink, read) = (reached.clone(), errors.clone(), memo_read.clone());
    let marked = define_task(
        TaskDefinition::new("test.marked", 1, |_| Ok(Json::from(json!({ "phase": "run" }))))
            .phase("run", move |_, runtime, context| {
                let (reached, sink, read) = (signal_reached.clone(), sink.clone(), read.clone());
                async move {
                    runtime.memo_or("kept", Json::from(json!(1)), &context).await?;
                    reached.resolve();
                    // Keep working after the signal: every later write of this run must reject.
                    runtime.signal().cancelled().await;
                    *read.lock().unwrap() = runtime.memo("kept").await?;
                    if let Err(error) = runtime.memo_or("late", Json::from(json!(2)), &support::context()).await {
                        sink.lock().unwrap().push(error.to_string());
                    }
                    let plain = support::context();
                    let commit = runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &plain);
                    if let Err(error) = commit.await {
                        sink.lock().unwrap().push(error.to_string());
                    }
                    Ok(())
                }
            })
            .abort(|_, runtime, context| async move {
                let change = |_, current: TaskRecord| async move {
                    assert!(current.abort_requested);
                    let memos = current.memos.map(|memos| Json::object(memos).to_serde());
                    assert_eq!(memos, Some(json!({ "kept": 1 })));
                    Ok(aborted_with("mark"))
                };
                runtime.commit(change, &context).await
            }),
    );
    let (harness, root, _) = open_root(vec![marked.clone()]).await;
    let id = start(&root, &marked, false).await;
    harness.resume().unwrap();
    reached.wait().await;
    assert_eq!(harness.abort_task(id, &context()).await.unwrap(), AbortTaskResult::Marked);
    assert_eq!(outcome(&harness, id).await, json!({ "status": "aborted", "reason": "mark" }));
    assert_eq!(memo_read.lock().unwrap().as_ref().map(Json::to_serde), Some(json!(1)));
    let mark = format!("Task {id} has a durable abort mark");
    assert_eq!(*errors.lock().unwrap(), [mark.clone(), mark]);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn signals_and_joins_the_run_before_returning_then_runs_the_abort_handler() {
    let reached = Deferred::default();
    let ended = Arc::new(Mutex::new(false));
    let (signal_reached, sink) = (reached.clone(), ended.clone());
    let signalled = one_step("test.signalled", move |_, runtime, _| {
        let (reached, sink) = (signal_reached.clone(), sink.clone());
        async move {
            reached.resolve();
            runtime.signal().cancelled().await;
            *sink.lock().unwrap() = true;
            Ok(())
        }
    });
    let (harness, root, _) = open_root(vec![signalled.clone()]).await;
    let id = start(&root, &signalled, false).await;
    harness.resume().unwrap();
    reached.wait().await;
    assert_eq!(harness.abort_task(id, &context()).await.unwrap(), AbortTaskResult::Marked);
    assert!(*ended.lock().unwrap());
    // Upstream sees `marked` here; the Rust abort handler may already have settled while this call queued.
    let again = harness.abort_task(id, &context()).await.unwrap();
    assert!(matches!(again, AbortTaskResult::Marked | AbortTaskResult::Terminal));
    assert_eq!(outcome(&harness, id).await, json!({ "status": "aborted", "reason": "test" }));
    harness.close(&context()).await.unwrap();
}
