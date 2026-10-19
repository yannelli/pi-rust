//! Port of upstream `test/harness-sleep.test.ts`: sleeping tasks, host timers, and wakeups.

mod support;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_chord::Json;
use pi_durable::harness::*;
use pi_durable::session::Tx;
use pi_durable::storage::MemoryStorage;
use pi_durable::storage::sqlite::open_sqlite_storage;
use pi_durable::tasks::TaskOptions;
use pi_durable::timers::{Disarm, TimerCallback, TimerOptions};
use pi_durable::*;
use serde_json::{Value, json};
use support::context;
use support::task_support::{aborted, aborted_with, completed, eventually, open_tasks_with};

type Clock = Arc<Mutex<f64>>;

fn clock(at: f64) -> Clock {
    Arc::new(Mutex::new(at))
}

fn set(clock: &Clock, at: f64) {
    *clock.lock().unwrap() = at;
}

/// Sleeps `delay` ms on the Harness clock, then completes with the time it resumed at; counts runs of `after`.
fn nap_task(delay: f64, runs: Arc<AtomicUsize>) -> Task {
    define_task(
        TaskDefinition::new("test.nap", 1, |_| Ok(Json::from(json!({ "phase": "start" }))))
            .phase("start", move |_, runtime, context| async move {
                let checkpoint = Json::from(json!({ "phase": "after", "slept": delay as i64 }));
                runtime.sleep_until(runtime.now() + delay, checkpoint, &context).await
            })
            .phase("after", move |_, runtime, context| {
                let runs = runs.clone();
                async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    let resumed = runtime.now() as i64;
                    runtime.commit(move |_, _| async move { Ok(completed(json!(resumed))) }, &context).await
                }
            })
            .abort(|_, runtime, context| async move {
                runtime.commit(|_, _| async { Ok(aborted_with("test")) }, &context).await
            }),
    )
}

fn nap(delay: f64) -> Task {
    nap_task(delay, Arc::default())
}

async fn open(
    storage: Arc<dyn Storage>,
    tasks: Vec<Task>,
    configure: impl FnOnce(HarnessOptions) -> HarnessOptions,
) -> support::task_support::OpenedTasks {
    open_tasks_with(storage, tasks, Arc::new(create_registry()), configure).await
}

async fn start(harness: &Harness, task: &Task, input: Value) -> TaskId {
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    let (task, id) = (task.clone(), root.id);
    let create =
        move |tx: Tx| async move { tx.create_task(task, Json::from(input), TaskOptions::conversation(id)).await };
    root.commit(create, &context()).await.unwrap()
}

async fn state_of(harness: &Harness, id: TaskId) -> Value {
    let record = harness.get_task(id, &context()).await.unwrap().unwrap();
    Value::from(&Json::from(serde_json::to_value(&record.state).unwrap()))
}

async fn status_of(harness: &Harness, id: TaskId) -> String {
    state_of(harness, id).await["status"].as_str().unwrap().to_owned()
}

async fn outcome(harness: &Harness, id: TaskId) -> Value {
    let record = harness.wait_for_task(id, &context()).await.unwrap();
    Value::from(&Json::from(serde_json::to_value(record.state.outcome()).unwrap()))
}

fn wake(at: Option<f64>, active: bool) -> Wake {
    Wake { active, at }
}

struct FakeTimer {
    delay_ms: f64,
    callback: Option<TimerCallback>,
    disarmed: Arc<AtomicBool>,
}

/// A host `set_timer` that records the timers that pace work and declines background ones.
#[derive(Clone, Default)]
struct FakeTimers(Arc<Mutex<Vec<FakeTimer>>>);

impl FakeTimers {
    fn set_timer(&self) -> impl Fn(TimerCallback, f64, TimerOptions) -> Option<Disarm> + Send + Sync + 'static {
        let armed = self.0.clone();
        move |callback, delay_ms, options| {
            if options.background {
                return None;
            }
            let disarmed = Arc::new(AtomicBool::new(false));
            armed.lock().unwrap().push(FakeTimer { delay_ms, callback: Some(callback), disarmed: disarmed.clone() });
            Some(Box::new(move || disarmed.store(true, Ordering::SeqCst)))
        }
    }

    /// Delays of every timer armed so far.
    fn armed(&self) -> Vec<f64> {
        self.0.lock().unwrap().iter().map(|timer| timer.delay_ms).collect()
    }

    /// Delays of the timers neither disarmed nor fired.
    fn live(&self) -> Vec<f64> {
        let timers = self.0.lock().unwrap();
        let live = timers.iter().filter(|timer| timer.callback.is_some() && !timer.disarmed.load(Ordering::SeqCst));
        live.map(|timer| timer.delay_ms).collect()
    }

    fn disarmed(&self, delay_ms: f64) -> bool {
        let timers = self.0.lock().unwrap();
        timers.iter().any(|timer| timer.delay_ms == delay_ms && timer.disarmed.load(Ordering::SeqCst))
    }

    /// Fire the first timer of `delay_ms` that has not fired, even when it was disarmed.
    fn fire(&self, delay_ms: f64) {
        let callback = {
            let mut timers = self.0.lock().unwrap();
            let timer = timers.iter_mut().find(|timer| timer.delay_ms == delay_ms && timer.callback.is_some());
            timer.and_then(|timer| timer.callback.take()).unwrap()
        };
        callback();
    }
}

/// Let spawned work and a few timer turns run.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(20)).await;
}

#[tokio::test]
async fn sleeps_without_an_invocation_and_resumes_at_its_checkpoint_once_due() {
    let now = clock(1_000.0);
    let wakes = Arc::new(Mutex::new(Vec::<Wake>::new()));
    let runs = Arc::new(AtomicUsize::new(0));
    let nap = nap_task(1_000.0, runs.clone());
    let (time, sink) = (now.clone(), wakes.clone());
    let opened = open(Arc::new(MemoryStorage::new()), vec![nap.clone()], move |options| {
        options.with_now(move || *time.lock().unwrap()).with_on_wake(move |wake| {
            sink.lock().unwrap().push(wake);
            Ok(())
        })
    })
    .await;
    let harness = opened.harness;
    let graph = harness.watch_task_graph(&context()).await.unwrap();
    graph.start(|_, _, _| async { Ok(()) }).unwrap();
    let id = start(&harness, &nap, Value::Null).await;
    harness.resume().unwrap();
    eventually(|| async { status_of(&harness, id).await == "sleeping" }).await;
    assert_eq!(
        state_of(&harness, id).await,
        json!({ "status": "sleeping", "checkpoint": { "phase": "after", "slept": 1000 }, "until": 2000 })
    );
    let inspection = harness.inspect(&context()).await.unwrap();
    let inspected = inspection.tasks.iter().find(|task| task.record.id == id).unwrap();
    assert_eq!(inspected.state, TaskInspectionState::Sleeping { until: 2_000.0 });
    let graph_state = || Value::from(&graph.value())["tasks"][id.to_string()]["state"].clone();
    eventually(|| async { graph_state()["status"] == "sleeping" }).await;
    assert_eq!(graph_state(), json!({ "status": "sleeping", "phase": "after", "until": 2000 }));
    eventually(|| async { !harness.wake().unwrap().active }).await;
    assert_eq!(harness.wake().unwrap(), wake(Some(2_000.0), false));
    settle().await;
    assert_eq!(wakes.lock().unwrap().last(), Some(&wake(Some(2_000.0), false)));

    // Early and repeated wakes run nothing.
    set(&now, 1_999.0);
    harness.resume().unwrap();
    harness.resume().unwrap();
    settle().await;
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert_eq!(status_of(&harness, id).await, "sleeping");

    set(&now, 2_000.0);
    harness.resume().unwrap();
    harness.resume().unwrap();
    assert_eq!(outcome(&harness, id).await, json!({ "status": "completed", "result": 2000 }));
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    settle().await;
    eventually(|| async { !harness.wake().unwrap().active }).await;
    assert_eq!(harness.wake().unwrap(), wake(None, false));
    let wakes = wakes.lock().unwrap().clone();
    assert_eq!(wakes.last(), Some(&wake(None, false)));
    // Only changes: idle at open, running, asleep until 2000, running again, idle.
    assert!(wakes.windows(2).all(|pair| pair[0] != pair[1]), "{wakes:?}");
    assert!(wakes.contains(&wake(None, true)));
    assert!(wakes.contains(&wake(Some(2_000.0), false)));
    graph.stop().await;
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn keeps_a_sleep_across_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite").to_string_lossy().into_owned();
    let now = clock(1_000.0);
    let nap = nap(60_000.0);
    let reopen = || async {
        let (storage, time) = (open_sqlite_storage(&path).await.unwrap(), now.clone());
        let configure =
            move |options: HarnessOptions| options.with_now(move || *time.lock().unwrap()).with_on_wake(|_| Ok(()));
        open(Arc::new(storage), vec![nap.clone()], configure).await.harness
    };
    let harness = reopen().await;
    let id = start(&harness, &nap, Value::Null).await;
    harness.resume().unwrap();
    eventually(|| async { status_of(&harness, id).await == "sleeping" }).await;
    harness.close(&context()).await.unwrap();

    let harness = reopen().await;
    // Known from storage before scheduling starts, once open's recovery pass is done.
    eventually(|| async { !harness.wake().unwrap().active }).await;
    assert_eq!(harness.wake().unwrap(), wake(Some(61_000.0), false));
    harness.resume().unwrap();
    settle().await;
    assert_eq!(status_of(&harness, id).await, "sleeping");
    harness.close(&context()).await.unwrap();

    set(&now, 61_000.0);
    let harness = reopen().await;
    assert_eq!(harness.wake().unwrap(), wake(None, true));
    harness.resume().unwrap();
    assert_eq!(outcome(&harness, id).await, json!({ "status": "completed", "result": 61000 }));
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn resumes_a_due_sleep_with_its_timer_without_on_wake_with_background_timers_declined() {
    let now = clock(1_000.0);
    let timers = FakeTimers::default();
    let nap = nap(500.0);
    let (time, set_timer) = (now.clone(), timers.set_timer());
    let opened = open(Arc::new(MemoryStorage::new()), vec![nap.clone()], move |options| {
        options.with_now(move || *time.lock().unwrap()).with_set_timer(set_timer)
    })
    .await;
    let harness = opened.harness;
    let id = start(&harness, &nap, Value::Null).await;
    harness.resume().unwrap();
    eventually(|| async { timers.live().contains(&500.0) }).await;
    assert_eq!(status_of(&harness, id).await, "sleeping");
    set(&now, 1_500.0);
    timers.fire(500.0);
    // Polled: `wait_for_task()` would call `resume()`, which runs due sleeps itself.
    eventually(|| async { status_of(&harness, id).await == "terminal" }).await;
    assert_eq!(state_of(&harness, id).await["outcome"], json!({ "status": "completed", "result": 1500 }));
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn runs_a_due_sleep_at_any_scheduling_pass_before_its_timer_fires() {
    let now = clock(1_000.0);
    let timers = FakeTimers::default();
    let nap = nap(500.0);
    let (time, set_timer) = (now.clone(), timers.set_timer());
    let opened = open(Arc::new(MemoryStorage::new()), vec![nap.clone()], move |options| {
        options.with_now(move || *time.lock().unwrap()).with_set_timer(set_timer)
    })
    .await;
    let harness = opened.harness;
    let id = start(&harness, &nap, Value::Null).await;
    harness.resume().unwrap();
    eventually(|| async { status_of(&harness, id).await == "sleeping" }).await;
    // The deadline passes, and an unrelated change starts a pass before the timer fires.
    set(&now, 1_500.0);
    opened.registry.install(Extension::new("unrelated")).unwrap();
    eventually(|| async { status_of(&harness, id).await == "terminal" }).await;
    assert_eq!(state_of(&harness, id).await["outcome"], json!({ "status": "completed", "result": 1500 }));
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn disarms_the_sleep_timer_when_the_sleep_is_aborted_and_when_the_harness_closes() {
    let timers = FakeTimers::default();
    let short = nap(500.0);
    let set_timer = timers.set_timer();
    let opened = open(Arc::new(MemoryStorage::new()), vec![short.clone()], move |options| {
        options.with_now(|| 1_000.0).with_set_timer(set_timer)
    })
    .await;
    let harness = opened.harness;
    let aborted = start(&harness, &short, Value::Null).await;
    harness.resume().unwrap();
    eventually(|| async { timers.live().contains(&500.0) }).await;
    harness.abort_task(aborted, &context()).await.unwrap();
    harness.wait_for_task(aborted, &context()).await.unwrap();
    eventually(|| async { timers.live().is_empty() }).await;
    let kept = start(&harness, &short, Value::Null).await;
    eventually(|| async { status_of(&harness, kept).await == "sleeping" }).await;
    eventually(|| async { timers.live() == [500.0] }).await;
    harness.close(&context()).await.unwrap();
    assert_eq!(timers.live(), Vec::<f64>::new());
}

#[tokio::test]
async fn disarms_an_invocations_timers_when_it_ends_and_a_late_fire_runs_nothing() {
    let fired = Arc::new(Mutex::new(Vec::<&str>::new()));
    let sink = fired.clone();
    let timed = define_task(
        TaskDefinition::new("test.timed", 1, |_| Ok(Json::from(json!({ "phase": "run" }))))
            .phase("run", move |_, runtime, context| {
                let sink = sink.clone();
                async move {
                    let _keep = runtime.set_timer(move || sink.lock().unwrap().push("late"), 60_000.0);
                    runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &context).await
                }
            })
            .abort(|_, _, _| async { Ok(()) }),
    );
    let timers = FakeTimers::default();
    let set_timer = timers.set_timer();
    let opened =
        open(Arc::new(MemoryStorage::new()), vec![timed.clone()], move |options| options.with_set_timer(set_timer))
            .await;
    let harness = opened.harness;
    let id = start(&harness, &timed, Value::Null).await;
    harness.resume().unwrap();
    harness.wait_for_task(id, &context()).await.unwrap();
    eventually(|| async { timers.disarmed(60_000.0) }).await;
    // A host timer that fires anyway: the ended invocation's callback does not run.
    timers.fire(60_000.0);
    assert!(fired.lock().unwrap().is_empty());
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn arms_no_timer_for_sleeps_with_on_wake() {
    let now = clock(1_000.0);
    let timers = FakeTimers::default();
    let nap = nap(500.0);
    let (time, set_timer) = (now.clone(), timers.set_timer());
    let opened = open(Arc::new(MemoryStorage::new()), vec![nap.clone()], move |options| {
        options.with_now(move || *time.lock().unwrap()).with_set_timer(set_timer).with_on_wake(|_| Ok(()))
    })
    .await;
    let harness = opened.harness;
    let id = start(&harness, &nap, Value::Null).await;
    harness.resume().unwrap();
    eventually(|| async { status_of(&harness, id).await == "sleeping" }).await;
    settle().await;
    assert_eq!(timers.armed(), Vec::<f64>::new());
    set(&now, 1_500.0);
    assert_eq!(harness.wake().unwrap(), wake(None, true));
    harness.resume().unwrap();
    assert_eq!(outcome(&harness, id).await["status"], "completed");
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn arms_a_foreground_timer_the_host_declines_with_a_tokio_timer_reporting_it_once() {
    let nap = nap(10.0);
    let opened =
        open(Arc::new(MemoryStorage::new()), vec![nap.clone()], |options| options.with_set_timer(|_, _, _| None)).await;
    let harness = opened.harness;
    let first = start(&harness, &nap, Value::Null).await;
    let second = start(&harness, &nap, Value::Null).await;
    harness.resume().unwrap();
    assert_eq!(outcome(&harness, first).await["status"], "completed");
    assert_eq!(outcome(&harness, second).await["status"], "completed");
    let reports: Vec<String> = opened.reports.lock().unwrap().iter().map(ToString::to_string).collect();
    assert_eq!(reports, ["HarnessOptions::set_timer declined a timer that paces work; using a Tokio timer"]);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn counts_neither_a_wait_on_a_sleeping_task_nor_an_outcome_held_over_one_as_active() {
    let child = nap(500.0);
    let owned = child.clone();
    let parent = define_task(
        TaskDefinition::new("test.parent", 1, |_| Ok(Json::from(json!({ "phase": "start" }))))
            .phase("start", move |task, runtime, context| {
                let child = owned.clone();
                async move {
                    let hold = task.input.get("hold").and_then(Json::as_bool).unwrap();
                    let change = move |tx: Tx, _| async move {
                        let child = tx.create_task(child, Json::Null, TaskOptions::owned_by(task.id)).await?;
                        let checkpoint = Json::from(json!({ "phase": "joined", "child": child }));
                        // Held: completes next, holding its outcome over the sleeping child. Otherwise waits for it.
                        Ok(Some(if hold {
                            TaskState::Running { checkpoint }
                        } else {
                            TaskState::Waiting { checkpoint, on: vec![child], policy: JoinPolicy::AllSettled }
                        }))
                    };
                    runtime.commit(change, &context).await
                }
            })
            .phase("joined", |_, runtime, context| async move {
                runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &context).await
            })
            .abort(|_, _, _| async { Ok(()) }),
    );
    for hold in [false, true] {
        let tasks = vec![parent.clone(), child.clone()];
        let configure = |options: HarnessOptions| options.with_now(|| 1_000.0).with_on_wake(|_| Ok(()));
        let harness = open(Arc::new(MemoryStorage::new()), tasks, configure).await.harness;
        let id = start(&harness, &parent, json!({ "hold": hold })).await;
        harness.resume().unwrap();
        let parked = if hold { "completing" } else { "waiting" };
        eventually(|| async { status_of(&harness, id).await == parked }).await;
        eventually(|| async { harness.wake().unwrap().at == Some(1_500.0) }).await;
        eventually(|| async { !harness.wake().unwrap().active }).await;
        assert_eq!(harness.wake().unwrap(), wake(Some(1_500.0), false));
        harness.close(&context()).await.unwrap();
    }
}

#[tokio::test]
async fn runs_the_abort_handler_of_a_sleeping_task_without_waiting_for_its_time() {
    let nap = nap(60_000.0);
    let configure = |options: HarnessOptions| options.with_now(|| 1_000.0).with_on_wake(|_| Ok(()));
    let harness = open(Arc::new(MemoryStorage::new()), vec![nap.clone()], configure).await.harness;
    let id = start(&harness, &nap, Value::Null).await;
    harness.resume().unwrap();
    eventually(|| async { status_of(&harness, id).await == "sleeping" }).await;
    harness.abort_task(id, &context()).await.unwrap();
    assert_eq!(outcome(&harness, id).await, json!({ "status": "aborted", "reason": "test" }));
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn rejects_a_sleep_until_something_that_is_not_a_time_and_a_sleep_in_an_abort_handler() {
    let errors = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = errors.clone();
    let invalid = define_task(
        TaskDefinition::new("test.invalid-sleep", 1, |_| Ok(Json::from(json!({ "phase": "run" }))))
            .phase("run", move |_, runtime, context| {
                let sink = sink.clone();
                async move {
                    for until in [f64::NAN, -1.0, f64::INFINITY] {
                        let next = Json::from(json!({ "phase": "next" }));
                        if let Err(error) = runtime.sleep_until(until, next, &context).await {
                            sink.lock().unwrap().push(error.to_string());
                        }
                    }
                    runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &context).await
                }
            })
            .phase("next", |_, _, _| async { Ok(()) })
            .abort(|_, _, _| async { Ok(()) }),
    );
    let sink = errors.clone();
    let aborting = define_task(
        TaskDefinition::new("test.sleep-in-abort", 1, |_| Ok(Json::from(json!({ "phase": "run" }))))
            .phase("run", |_, runtime, _| async move {
                aborted(runtime.signal()).await;
                Ok(())
            })
            .abort(move |_, runtime, context| {
                let sink = sink.clone();
                async move {
                    let next = Json::from(json!({ "phase": "run" }));
                    if let Err(error) = runtime.sleep_until(runtime.now() + 10.0, next, &context).await {
                        sink.lock().unwrap().push(error.to_string());
                    }
                    runtime.commit(|_, _| async { Ok(aborted_with("test")) }, &context).await
                }
            }),
    );
    let harness =
        open(Arc::new(MemoryStorage::new()), vec![invalid.clone(), aborting.clone()], |options| options).await.harness;
    let id = start(&harness, &invalid, Value::Null).await;
    let aborting = start(&harness, &aborting, Value::Null).await;
    harness.resume().unwrap();
    assert_eq!(outcome(&harness, id).await["status"], "completed");
    harness.abort_task(aborting, &context()).await.unwrap();
    assert_eq!(outcome(&harness, aborting).await["status"], "aborted");
    assert_eq!(
        *errors.lock().unwrap(),
        [
            format!("Task {id} cannot sleep until NaN: not a time in milliseconds"),
            format!("Task {id} cannot sleep until -1: not a time in milliseconds"),
            format!("Task {id} cannot sleep until Infinity: not a time in milliseconds"),
            format!("Abort handler of task {aborting} cannot sleep"),
        ]
    );
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn does_not_count_a_task_without_a_definition_as_active() {
    let missing = nap(10.0);
    let harness = open(Arc::new(MemoryStorage::new()), Vec::new(), |options| options).await.harness;
    // Created through a definition the registry does not hold: nothing can run it.
    start(&harness, &missing, Value::Null).await;
    eventually(|| async { !harness.wake().unwrap().active }).await;
    assert_eq!(harness.wake().unwrap(), wake(None, false));
    harness.resume().unwrap();
    settle().await;
    eventually(|| async { !harness.wake().unwrap().active }).await;
    assert_eq!(harness.wake().unwrap(), wake(None, false));
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn reports_a_failing_on_wake_and_offers_the_same_wake_again_at_the_next_pass() {
    let now = clock(1_000.0);
    let wakes = Arc::new(Mutex::new(Vec::<Wake>::new()));
    let failing = Arc::new(AtomicBool::new(true));
    let nap = nap(1_000.0);
    let (time, sink, fails) = (now.clone(), wakes.clone(), failing.clone());
    let opened = open(Arc::new(MemoryStorage::new()), vec![nap.clone()], move |options| {
        options.with_now(move || *time.lock().unwrap()).with_on_wake(move |wake| {
            sink.lock().unwrap().push(wake);
            if fails.load(Ordering::SeqCst) && wake.at.is_some() {
                return Err(DurableError::error("alarm failed"));
            }
            Ok(())
        })
    })
    .await;
    let harness = opened.harness;
    let id = start(&harness, &nap, Value::Null).await;
    harness.resume().unwrap();
    eventually(|| async { wakes.lock().unwrap().contains(&wake(Some(2_000.0), false)) }).await;
    eventually(|| async { opened.reports.lock().unwrap().iter().any(|error| error.to_string() == "alarm failed") })
        .await;
    // The host recovers; an unrelated change offers the same, undelivered wake again.
    failing.store(false, Ordering::SeqCst);
    let offered = wakes.lock().unwrap().len();
    opened.registry.install(Extension::new("unrelated")).unwrap();
    eventually(|| async { wakes.lock().unwrap().len() > offered }).await;
    assert_eq!(wakes.lock().unwrap().last(), Some(&wake(Some(2_000.0), false)));
    set(&now, 2_000.0);
    harness.resume().unwrap();
    harness.wait_for_task(id, &context()).await.unwrap();
    eventually(|| async { wakes.lock().unwrap().last() == Some(&wake(None, false)) }).await;
    harness.close(&context()).await.unwrap();
}
