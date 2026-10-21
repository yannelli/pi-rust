//! Port of upstream `test/harness-nested-tools-restart.test.ts` (nested tool calls across a restart). This file holds
//! the helpers and the abandonment cases; `recovery.rs` holds the replay-safe and child task cases.

#[path = "../support/mod.rs"]
mod support;

mod earlier_runs;
mod recovery;

use std::sync::{Arc, Mutex};

use pi_ai::{Message, ToolResultMessage};
use pi_chord::{Context, Json};
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::harness::tool::{ToolHookCall, ToolHooks, tool_task};
use pi_durable::harness::*;
use pi_durable::storage::sqlite::open_sqlite_storage;
use pi_durable::tasks::TaskOptions;
use pi_durable::*;
use serde_json::{Value, json};
use support::Deferred;
use support::chat::{add_tool, answer, call};
use support::harness_support::*;
use support::task_support::{aborted, eventually};

fn parameters() -> Value {
    json!({ "type": "object", "properties": { "text": { "type": "string" } } })
}

pub fn tool<F, Fut>(name: &str, execute: F) -> ToolRegistration
where
    F: Fn(pi_ai::JsonObject, ToolExecutionApi, Context) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = DurableResult<ToolExecutionResult>> + Send + 'static,
{
    define_tool(name, name, parameters(), execute)
}

pub fn sqlite_path(directory: &tempfile::TempDir) -> String {
    directory.path().join("session.sqlite").to_string_lossy().into_owned()
}

/// Open the SQLite Harness without starting scheduling.
pub async fn open(path: &str, setup: &ChatSetup) -> (Harness, Conversation) {
    open_chat(Arc::new(open_sqlite_storage(path).await.unwrap()), setup).await
}

pub async fn nested_tasks(harness: &Harness) -> Vec<TaskRecord> {
    let query = TaskQuery { kind: Some("pi.tool".into()), ..TaskQuery::default() };
    let tasks = harness.commit(move |tx| async move { tx.scan_tasks(query, 100, None).await }, &ctx()).await.unwrap();
    tasks.items.into_iter().filter(|task| task.input["kind"] == Json::from("nested")).collect()
}

pub fn results(entries: &[EntryRecord]) -> Vec<ToolResultMessage> {
    let messages = entries.iter().filter(|entry| entry.kind == "pi.tool-result");
    messages
        .filter_map(|entry| match entry.model.as_ref()?.first()? {
            Message::ToolResult(result) => Some(result.clone()),
            _ => None,
        })
        .collect()
}

pub fn codes(result: Option<&NestedToolExecutionResult>) -> Vec<String> {
    result
        .map(|result| result.diagnostics.iter().filter_map(|diagnostic| diagnostic.code.clone()).collect())
        .unwrap_or_default()
}

pub async fn cancelled(context: &Context) -> DurableResult<ToolExecutionResult> {
    let signal = context.abort_signal().unwrap();
    aborted(signal.clone()).await;
    Err(signal.reason().map_or_else(DurableError::aborted, DurableError::from))
}

pub async fn settle(harness: &Harness, id: SubmissionId) -> SubmissionStatus {
    harness.submission(id, &ctx()).await.unwrap().unwrap().wait(&ctx()).await.unwrap().status
}

pub async fn submit(root: &Conversation) -> SubmissionId {
    root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().id
}

fn aborted_state() -> Option<TaskState> {
    Some(TaskState::Terminal { outcome: TaskOutcome::Aborted { reason: None, result: None } })
}

/// A task that records its phases and abort reasons, signals `started`, and waits until aborted.
pub fn worker(name: &str, log: &Arc<Mutex<Vec<String>>>, started: &Deferred) -> Task {
    let (phase_log, abort_log, started) = (log.clone(), log.clone(), started.clone());
    define_task(
        TaskDefinition::new(name, 1, |_| Ok(Json::from(json!({ "phase": "work" }))))
            .phase("work", move |_, runtime, _| {
                phase_log.lock().unwrap().push("work".into());
                let started = started.clone();
                async move {
                    started.resolve();
                    aborted(runtime.signal()).await;
                    Ok(())
                }
            })
            .abort(move |task, runtime, context| {
                let reason = task.abort_reason.map(|reason| serde_json::to_value(reason).unwrap());
                let reason = reason.and_then(|reason| reason.as_str().map(str::to_owned)).unwrap_or_default();
                abort_log.lock().unwrap().push(format!("abort {reason}"));
                async move { runtime.commit(|_, _| async { Ok(aborted_state()) }, &context).await }
            }),
    )
}

/// A `spawn` tool that creates `task` owned by its call and waits for it, called by a `batch` tool as a nested call.
pub fn spawn_and_batch(setup: &ChatSetup, task: &Task) -> Arc<Mutex<Option<TaskId>>> {
    let created = Arc::new(Mutex::new(None));
    let (sink, task) = (created.clone(), task.clone());
    add_tool(
        setup,
        tool("spawn", move |_, api, context| {
            let (sink, task) = (sink.clone(), task.clone());
            async move {
                let id = api.create_task(task, Json::Null, TaskOptions::owned_by(api.task_id), &context).await?;
                *sink.lock().unwrap() = Some(id);
                api.wait_for_task(id, &context).await?;
                Ok(ToolExecutionResult::default())
            }
        }),
    );
    add_tool(
        setup,
        tool("batch", |_, api, context| async move {
            api.execute_tool("spawn", Default::default(), &context, ExecuteToolOptions::default()).await?;
            Ok(ToolExecutionResult::default())
        }),
    );
    setup.faux.set_responses([call("batch", "c1"), answer("done")]);
    created
}

pub fn outcome_status(record: &TaskRecord) -> Option<&'static str> {
    match &record.state {
        TaskState::Terminal { outcome } => Some(outcome.status()),
        _ => None,
    }
}

#[tokio::test]
async fn abandons_a_replay_unsafe_caller_s_unfinished_nested_calls_before_they_run_again() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let setup = chat_setup(Default::default());
    let runs = Arc::new(Mutex::new((0, 0)));
    let (running, gated) = (Deferred::default(), Deferred::default());
    let (count, signal) = (runs.clone(), running.clone());
    // Replay-safe, so only the abandonment keeps it from rerunning.
    let started = tool("started", move |_, api, context| {
        let (count, running) = (count.clone(), signal.clone());
        async move {
            count.lock().unwrap().0 += 1;
            api.output("partial\n")?;
            api.details(Json::from(json!({ "step": 1 })), &context).await?;
            running.resolve();
            cancelled(&context).await
        }
    });
    add_tool(&setup, started.with_replay(ToolReplay::Safe));
    let count = runs.clone();
    add_tool(
        &setup,
        tool("gated", move |_, _, _| {
            count.lock().unwrap().1 += 1;
            async { Ok(ToolExecutionResult::default()) }
        }),
    );
    // Holds `gated` in its `call` phase, before intent, until the Harness closes.
    let signal = gated.clone();
    let hooks = ToolHooks {
        before_tool: Some(Arc::new(move |call: ToolHookCall, _, context: Context| {
            let gated = signal.clone();
            Box::pin(async move {
                if call.call.name != "gated" {
                    return Ok(None);
                }
                gated.resolve();
                cancelled(&context).await.map(|_| None)
            })
        })),
        after_tool: None,
    };
    setup.registry.install(Extension::new("hooks").hook(hook(&tool_task(), hooks))).unwrap();
    add_tool(
        &setup,
        tool("batch", |_, api, context| async move {
            let first = api.execute_tool("started", Default::default(), &context, ExecuteToolOptions::default());
            let second = api.execute_tool("gated", Default::default(), &context, ExecuteToolOptions::default());
            let (first, second) = futures::join!(first, second);
            first?;
            second?;
            Ok(ToolExecutionResult::default())
        }),
    );
    setup.faux.set_responses([call("batch", "c1"), answer("done")]);
    let (harness, root) = open(&path, &setup).await;
    let id = submit(&root).await;
    running.wait().await;
    gated.wait().await;
    harness.close(&ctx()).await.unwrap();

    let (harness, root) = open(&path, &setup).await;
    let ends = Arc::new(Mutex::new(Vec::<Value>::new()));
    let stream = watch_events(&harness, root.id, &ctx()).await.unwrap();
    let sink = ends.clone();
    stream
        .start(move |batch, _| {
            let found = batch.iter().filter(|event| event["type"] == Json::from("tool_execution_end")).map(Value::from);
            sink.lock().unwrap().extend(found);
            async { Ok(()) }
        })
        .unwrap();
    harness.resume().unwrap();
    assert_eq!(settle(&harness, id).await, SubmissionStatus::Done);
    harness.wait_for_idle(&ctx()).await.unwrap();
    stream.stop().await;
    assert_eq!(*runs.lock().unwrap(), (1, 0));
    let tasks = nested_tasks(&harness).await;
    assert_eq!(tasks.len(), 2);
    for task in &tasks {
        assert_eq!(
            (task.abandon_on_restart, task.abort_requested, task.abort_reason),
            (Some(true), true, Some(AbortReason::Restart))
        );
        assert_eq!(outcome_status(task), Some("aborted"));
    }
    let ends = ends.lock().unwrap().clone();
    let end = |call_id: &str| -> Option<NestedToolExecutionResult> {
        let event = ends.iter().find(|event| event["toolCallId"] == call_id)?;
        serde_json::from_value(event["result"].clone()).ok()
    };
    let first = end("c1/1");
    assert_eq!(codes(first.as_ref()), ["interrupted"]);
    let first = first.unwrap();
    assert!(first.is_error);
    assert_eq!(first.details, Some(json!({ "step": 1 }).into()));
    assert_eq!(first.structured_output, None);
    assert_eq!(codes(end("c1/2").as_ref()), ["abandoned"]);
    // The caller reports the interruption.
    let result = results(&all_entries(&root).await).remove(0);
    assert_eq!((result.tool_call_id.as_str(), result.is_error), ("c1", true));
    assert_eq!(harness.snapshot(&LIVE_DOC, root.id, &ctx()).await.unwrap(), Some(Json::from(json!({}))));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn abandons_the_work_an_abandoned_nested_call_owns_which_never_runs_a_phase_again() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let setup = chat_setup(Default::default());
    let (log, started) = (Arc::new(Mutex::new(Vec::new())), Deferred::default());
    let task = worker("test.worker", &log, &started);
    setup.registry.install(Extension::new("worker").task(task.clone())).unwrap();
    let created = spawn_and_batch(&setup, &task);
    let (harness, root) = open(&path, &setup).await;
    let id = submit(&root).await;
    started.wait().await;
    harness.close(&ctx()).await.unwrap();

    let (harness, _) = open(&path, &setup).await;
    harness.resume().unwrap();
    assert_eq!(settle(&harness, id).await, SubmissionStatus::Done);
    harness.wait_for_idle(&ctx()).await.unwrap();
    // The phase never ran again, and the cascade passed the restart reason on.
    assert_eq!(*log.lock().unwrap(), ["work", "abort restart"]);
    let worker = created.lock().unwrap().unwrap();
    assert_eq!(outcome_status(&harness.get_task(worker, &ctx()).await.unwrap().unwrap()), Some("aborted"));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn keeps_an_abandoned_task_whose_definition_is_missing_waiting_until_it_is_installed_then_runs_its_abort() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let setup = chat_setup(Default::default());
    let (log, started) = (Arc::new(Mutex::new(Vec::new())), Deferred::default());
    let task = worker("test.late", &log, &started);
    setup.registry.install(Extension::new("late").task(task.clone())).unwrap();
    let created = spawn_and_batch(&setup, &task);
    let (harness, root) = open(&path, &setup).await;
    let id = submit(&root).await;
    started.wait().await;
    harness.close(&ctx()).await.unwrap();

    setup.registry.uninstall("late").unwrap();
    let (harness, _) = open(&path, &setup).await;
    harness.resume().unwrap();
    let worker = created.lock().unwrap().unwrap();
    eventually(|| async {
        let inspection = harness.inspect(&ctx()).await.unwrap();
        let found = inspection.tasks.into_iter().find(|task| task.record.id == worker);
        found.is_some_and(|task| matches!(task.state, TaskInspectionState::Blocked { .. }))
    })
    .await;
    assert_eq!(outcome_status(&harness.get_task(worker, &ctx()).await.unwrap().unwrap()), None);
    assert_eq!(*log.lock().unwrap(), ["work"]);
    setup.registry.install(Extension::new("late").task(task)).unwrap();
    assert_eq!(settle(&harness, id).await, SubmissionStatus::Done);
    assert_eq!(*log.lock().unwrap(), ["work", "abort restart"]);
    assert_eq!(outcome_status(&harness.get_task(worker, &ctx()).await.unwrap().unwrap()), Some("aborted"));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn orphans_an_abandoned_task_whose_definition_is_missing_once_an_abort_is_requested() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let setup = chat_setup(Default::default());
    let (log, started) = (Arc::new(Mutex::new(Vec::new())), Deferred::default());
    let task = worker("test.gone", &log, &started);
    setup.registry.install(Extension::new("gone").task(task.clone())).unwrap();
    let created = spawn_and_batch(&setup, &task);
    let (harness, root) = open(&path, &setup).await;
    let id = submit(&root).await;
    started.wait().await;
    harness.close(&ctx()).await.unwrap();

    setup.registry.uninstall("gone").unwrap();
    let (harness, root) = open(&path, &setup).await;
    harness.resume().unwrap();
    let worker = created.lock().unwrap().unwrap();
    eventually(|| async {
        let record = harness.get_task(worker, &ctx()).await.unwrap();
        record.is_some_and(|record| record.abort_reason == Some(AbortReason::Restart))
    })
    .await;
    root.abort(&ctx(), false).await.unwrap();
    let record = harness.get_task(worker, &ctx()).await.unwrap().unwrap();
    assert_eq!(record.state.outcome(), Some(&TaskOutcome::Orphaned { reason: "missing_task".into() }));
    assert_eq!(settle(&harness, id).await, SubmissionStatus::Unanswered);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn marks_nothing_when_the_reopened_harness_never_starts_scheduling() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let setup = chat_setup(Default::default());
    let running = Deferred::default();
    let signal = running.clone();
    add_tool(
        &setup,
        tool("hang", move |_, _, context| {
            let running = signal.clone();
            async move {
                running.resolve();
                cancelled(&context).await
            }
        }),
    );
    add_tool(
        &setup,
        tool("batch", |_, api, context| async move {
            api.execute_tool("hang", Default::default(), &context, ExecuteToolOptions::default()).await?;
            Ok(ToolExecutionResult::default())
        }),
    );
    setup.faux.set_responses([call("batch", "c1"), answer("done")]);
    let (harness, root) = open(&path, &setup).await;
    submit(&root).await;
    running.wait().await;
    harness.close(&ctx()).await.unwrap();

    // An inspection-only open: no resume, no submission.
    let (harness, _) = open(&path, &setup).await;
    assert_eq!(harness.inspect(&ctx()).await.unwrap().scheduling, Scheduling::Paused);
    harness.close(&ctx()).await.unwrap();

    let (harness, _) = open(&path, &setup).await;
    let nested = nested_tasks(&harness).await.remove(0);
    assert_eq!((nested.abandon_on_restart, nested.abort_requested), (Some(true), false));
    assert_eq!(nested.state.status(), TaskStatus::Pending);
    harness.close(&ctx()).await.unwrap();
}
