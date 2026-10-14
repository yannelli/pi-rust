//! The remaining upstream "task abort" cases.

use std::sync::atomic::{AtomicUsize, Ordering};

use super::phases::{checkpoint, gated, until_status};
use super::support::task_support::{aborted, open_tasks};
use super::support::{ControlledStorage, Gate};
use super::*;

type Handle<T> = tokio::task::JoinHandle<DurableResult<T>>;

/// A one-phase task with its own abort handler.
fn step_with_abort<F, Fut, A, AFut>(name: &str, run: F, abort: A) -> Task
where
    F: Fn(TaskRecord, TaskRuntime, pi_chord::Context) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = DurableResult<()>> + Send + 'static,
    A: Fn(TaskRecord, TaskRuntime, pi_chord::Context) -> AFut + Send + Sync + 'static,
    AFut: Future<Output = DurableResult<()>> + Send + 'static,
{
    define_task(
        TaskDefinition::new(name, 1, |_| Ok(Json::from(json!({ "phase": "run" })))).phase("run", run).abort(abort),
    )
}

/// Start `abort_task` and return once its mark is durable, before it has joined the run.
pub async fn mark_durably(harness: &Harness, id: TaskId, context: pi_chord::Context) -> Handle<AbortTaskResult> {
    let aborter = harness.clone();
    let aborting = tokio::spawn(async move { aborter.abort_task(id, &context).await });
    let probe = harness.clone();
    support::task_support::eventually(|| {
        let harness = probe.clone();
        async move { harness.get_task(id, &support::context()).await.unwrap().unwrap().abort_requested }
    })
    .await;
    aborting
}

#[tokio::test]
async fn starts_no_further_phase_after_a_mark_that_lands_during_a_phase_with_progress() {
    let (reached, proceed) = (Deferred::default(), Deferred::default());
    let phases = Arc::new(Mutex::new(Vec::new()));
    let (signal_reached, held, one_sink, two_sink, abort_sink) =
        (reached.clone(), proceed.clone(), phases.clone(), phases.clone(), phases.clone());
    let two = define_task(
        TaskDefinition::new("test.mark-boundary", 1, |_| Ok(Json::from(json!({ "phase": "one" }))))
            .phase("one", move |_, runtime, context| {
                one_sink.lock().unwrap().push("one");
                let (reached, proceed) = (signal_reached.clone(), held.clone());
                async move {
                    runtime.commit(|_, _| async { Ok(checkpoint("two")) }, &context).await?;
                    reached.resolve();
                    // Ignores the signal and returns normally after the mark.
                    proceed.wait().await;
                    Ok(())
                }
            })
            .phase("two", move |_, _, _| {
                two_sink.lock().unwrap().push("two");
                async { Ok(()) }
            })
            .abort(move |_, runtime, context| {
                abort_sink.lock().unwrap().push("abort");
                async move { runtime.commit(|_, _| async { Ok(aborted_with("boundary")) }, &context).await }
            }),
    );
    let (harness, root, _) = open_root(vec![two.clone()]).await;
    let id = start(&root, &two, false).await;
    harness.resume().unwrap();
    reached.wait().await;
    let aborting = mark_durably(&harness, id, context()).await;
    proceed.resolve();
    assert_eq!(aborting.await.unwrap().unwrap(), AbortTaskResult::Marked);
    assert_eq!(outcome(&harness, id).await, json!({ "status": "aborted", "reason": "boundary" }));
    assert_eq!(*phases.lock().unwrap(), ["one", "abort"]);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn aborts_waiting_work_before_its_wait_ends_and_faults_abort_handlers_that_throw_or_settle_nothing() {
    let gate = Deferred::default();
    let first_task = gated("test.dependency", gate.clone());
    let first: Arc<Mutex<Option<TaskId>>> = Arc::default();
    let wait = |first: Arc<Mutex<Option<TaskId>>>| {
        move |_, runtime: TaskRuntime, context: pi_chord::Context| {
            let on = vec![first.lock().unwrap().unwrap()];
            async move {
                let checkpoint = Json::from(json!({ "phase": "run" }));
                let state = Some(TaskState::Waiting { checkpoint, on, policy: JoinPolicy::AllSettled });
                runtime.commit(move |_, _| async move { Ok(state) }, &context).await
            }
        }
    };
    let lazy = step_with_abort("test.lazy-abort", wait(first.clone()), |_, _, _| async { Ok(()) });
    let throwing = step_with_abort("test.throwing-abort", wait(first.clone()), |_, _, _| async {
        Err(DurableError::error("abort failed"))
    });
    let (harness, root, _) = open_root(vec![first_task.clone(), lazy.clone(), throwing.clone()]).await;
    let first_id = start(&root, &first_task, false).await;
    *first.lock().unwrap() = Some(first_id);
    let lazy = start(&root, &lazy, false).await;
    let throwing = start(&root, &throwing, false).await;
    harness.resume().unwrap();
    until_status(&harness, throwing, TaskStatus::Waiting).await;
    until_status(&harness, lazy, TaskStatus::Waiting).await;
    harness.abort_task(lazy, &context()).await.unwrap();
    harness.abort_task(throwing, &context()).await.unwrap();
    let message = format!("Abort handler of task {lazy} returned without a terminal outcome");
    assert_eq!(outcome(&harness, lazy).await, json!({ "status": "faulted", "error": { "message": message } }));
    assert_eq!(
        outcome(&harness, throwing).await,
        json!({ "status": "faulted", "error": { "message": "abort failed" } })
    );
    let record = harness.get_task(first_id, &context()).await.unwrap().unwrap();
    assert_eq!(record.state.status(), TaskStatus::Running);
    gate.resolve();
    harness.wait_for_task(first_id, &context()).await.unwrap();
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn does_not_signal_a_running_abort_handler_when_aborted_again_and_a_cancelled_caller_leaves_the_mark_durable() {
    let (reached, proceed, run_release) = (Deferred::default(), Deferred::default(), Deferred::default());
    let signalled: Arc<Mutex<Option<bool>>> = Arc::default();
    let aborts = Arc::new(AtomicUsize::new(0));
    let release = run_release.clone();
    let (signal_reached, held, seen, count) = (reached.clone(), proceed.clone(), signalled.clone(), aborts.clone());
    let run = step_with_abort(
        "test.abort-again",
        move |_, _, _| {
            let release = release.clone();
            // Ignores the signal, so the first caller is still joining when it gives up.
            async move {
                release.wait().await;
                Ok(())
            }
        },
        move |_, runtime, context| {
            count.fetch_add(1, Ordering::SeqCst);
            let (reached, proceed, seen) = (signal_reached.clone(), held.clone(), seen.clone());
            async move {
                reached.resolve();
                proceed.wait().await;
                *seen.lock().unwrap() = Some(runtime.signal().aborted());
                runtime.commit(|_, _| async { Ok(aborted_with("once")) }, &context).await
            }
        },
    );
    let (harness, root, _) = open_root(vec![run.clone()]).await;
    let id = start(&root, &run, false).await;
    harness.resume().unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    // This caller gives up while joining; the mark and the abort invocation are unaffected.
    let controller = AbortController::new();
    let cancelled = mark_durably(&harness, id, with_abort_signal(&controller.signal(), &context())).await;
    controller.abort(Some(ChordError::error("caller gave up")));
    assert!(cancelled.await.unwrap().unwrap_err().to_string().contains("caller gave up"));
    run_release.resolve();
    reached.wait().await;
    assert_eq!(harness.abort_task(id, &context()).await.unwrap(), AbortTaskResult::Marked);
    proceed.resolve();
    assert_eq!(outcome(&harness, id).await, json!({ "status": "aborted", "reason": "once" }));
    assert_eq!(*signalled.lock().unwrap(), Some(false));
    assert_eq!(aborts.load(Ordering::SeqCst), 1);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn keeps_a_terminal_outcome_committed_by_an_abort_handler_that_throws_afterwards() {
    let run = step_with_abort(
        "test.abort-then-throw",
        |_, runtime, _| async move {
            aborted(runtime.signal()).await;
            Ok(())
        },
        |_, runtime, context| async move {
            runtime.commit(|_, _| async { Ok(aborted_with("done")) }, &context).await?;
            Err(DurableError::error("after terminal"))
        },
    );
    let (harness, root, _) = open_root(vec![run.clone()]).await;
    let id = start(&root, &run, false).await;
    harness.resume().unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    harness.abort_task(id, &context()).await.unwrap();
    assert_eq!(outcome(&harness, id).await, json!({ "status": "aborted", "reason": "done" }));
    harness.close(&context()).await.unwrap();
}

async fn open_controlled(tasks: Vec<Task>) -> (Arc<ControlledStorage>, Harness, Conversation) {
    let storage = Arc::new(ControlledStorage::default());
    let harness = open_tasks(storage.clone(), tasks).await.harness;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    (storage, harness, root)
}

#[tokio::test]
async fn never_starts_phase_one_when_the_abort_lands_while_the_reservation_settles() {
    let ran = Arc::new(Mutex::new(false));
    let sink = ran.clone();
    let reserved = one_step("test.reserved", move |_, runtime, _| {
        *sink.lock().unwrap() = true;
        async move {
            aborted(runtime.signal()).await;
            Ok(())
        }
    });
    let (storage, harness, root) = open_controlled(vec![reserved.clone()]).await;
    let id = start(&root, &reserved, false).await;
    let held = storage.hold_commits();
    harness.resume().unwrap();
    held.entered().await;
    // The reservation commit is in storage; the abort mark commit queues behind it.
    let aborter = harness.clone();
    let aborting = tokio::spawn(async move { aborter.abort_task(id, &context()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    held.release();
    assert_eq!(aborting.await.unwrap().unwrap(), AbortTaskResult::Marked);
    assert_eq!(outcome(&harness, id).await, json!({ "status": "aborted", "reason": "test" }));
    assert!(!*ran.lock().unwrap());
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn lets_an_abort_mark_win_over_a_fault_that_races_it() {
    let storage = Arc::new(ControlledStorage::default());
    let held: Arc<Mutex<Option<Gate>>> = Arc::default();
    let marking: Arc<Mutex<Option<Handle<AbortTaskResult>>>> = Arc::default();
    let harness_slot: Arc<Mutex<Option<Harness>>> = Arc::default();
    let (store, held_slot, mark_slot, harnesses) =
        (storage.clone(), held.clone(), marking.clone(), harness_slot.clone());
    let racing = one_step("test.racing-fault", move |task, _, _| {
        let (storage, held_slot, mark_slot) = (store.clone(), held_slot.clone(), mark_slot.clone());
        let harness = harnesses.lock().unwrap().clone().unwrap();
        async move {
            let gate = storage.hold_commits();
            *held_slot.lock().unwrap() = Some(gate.clone());
            let id = task.id;
            *mark_slot.lock().unwrap() = Some(tokio::spawn(async move { harness.abort_task(id, &context()).await }));
            gate.entered().await;
            // The mark is in storage but not committed when the handler throws, so the fault commit queues behind it.
            Err(DurableError::error("would fault"))
        }
    });
    let harness = open_tasks(storage.clone(), vec![racing.clone()]).await.harness;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    *harness_slot.lock().unwrap() = Some(harness.clone());
    let id = start(&root, &racing, false).await;
    harness.resume().unwrap();
    let ready = held.clone();
    eventually(move || ready.lock().unwrap().is_some()).await;
    let gate = held.lock().unwrap().clone().unwrap();
    gate.entered().await;
    tokio::time::sleep(Duration::from_millis(5)).await;
    gate.release();
    assert_eq!(outcome(&harness, id).await, json!({ "status": "aborted", "reason": "test" }));
    let marking = marking.lock().unwrap().take().unwrap();
    assert_eq!(marking.await.unwrap().unwrap(), AbortTaskResult::Marked);
    harness.close(&context()).await.unwrap();
}
