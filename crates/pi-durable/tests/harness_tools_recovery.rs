//! Port of upstream `test/harness-tools-recovery.test.ts`.

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pi_ai::{Message, ToolResultMessage};
use pi_chord::{Context, Json};
use pi_durable::env::{LocalEnvOptions, LocalExecutionEnv, SharedEnv};
use pi_durable::harness::generation::{GenerationHooks, generation_task};
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::harness::tool::{ToolHookCall, ToolHooks, tool_task};
use pi_durable::harness::*;
use pi_durable::storage::sqlite::open_sqlite_storage;
use pi_durable::testing::faux::{FauxMessageOptions, faux_assistant_message, faux_tool_call_with_id};
use pi_durable::tools::{BashToolOptions, create_bash_tool};
use pi_durable::{EntryRecord, SubmissionId, SubmissionStatus, TaskId};
use serde_json::{Value, json};
use support::Deferred;
use support::chat::{add_tool, answer, call, no_parameters};
use support::harness_support::*;
use support::task_support::aborted;

async fn open(path: &str, setup: &ChatSetup, env: Option<SharedEnv>) -> (Harness, Conversation) {
    let options = match env {
        Some(env) => setup.options().with_shared_env(env),
        None => setup.options(),
    };
    let opened = open_chat_with(Arc::new(open_sqlite_storage(path).await.unwrap()), setup, options).await;
    opened.0.resume().unwrap();
    opened
}

fn sqlite_path(directory: &tempfile::TempDir) -> String {
    directory.path().join("session.sqlite").to_string_lossy().into_owned()
}

fn results(entries: &[EntryRecord]) -> Vec<ToolResultMessage> {
    entries
        .iter()
        .filter(|entry| entry.kind == "pi.tool-result")
        .filter_map(|entry| match entry.model.as_ref()?.first()? {
            Message::ToolResult(result) => Some(result.clone()),
            _ => None,
        })
        .collect()
}

fn text(message: &ToolResultMessage) -> String {
    let parts: Vec<&str> = message.content.iter().map(|block| block.as_text().unwrap_or_default()).collect();
    parts.join("|")
}

async fn tool_task_id(harness: &Harness) -> TaskId {
    let read = || async {
        let live = harness.snapshot(&LIVE_DOC, pi_durable::ROOT_CONVERSATION_ID, &ctx()).await.unwrap();
        live.and_then(|live| live["tools"][0]["taskId"].as_u64()).map(TaskId)
    };
    wait_for(|| async { read().await.is_some() }).await;
    read().await.unwrap()
}

async fn settle(harness: &Harness, id: SubmissionId) -> SubmissionStatus {
    harness.submission(id, &ctx()).await.unwrap().unwrap().wait(&ctx()).await.unwrap().status
}

async fn submit(root: &Conversation) -> SubmissionId {
    root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().id
}

async fn cancelled(context: &Context) {
    aborted(context.abort_signal().unwrap()).await;
}

/// A tool that writes output, then blocks until its invocation is cancelled the first time it runs. `started`
/// resolves once the output is durable.
fn blocking_tool(name: &str) -> (ToolRegistration, Deferred, Arc<AtomicUsize>) {
    let (started, runs) = (Deferred::default(), Arc::new(AtomicUsize::new(0)));
    let (signal, count) = (started.clone(), runs.clone());
    let tool = define_tool(name, name, no_parameters(), move |_, api, context| {
        let (started, run) = (signal.clone(), count.fetch_add(1, Ordering::SeqCst) + 1);
        async move {
            api.output(&format!("run {run}\n"))?;
            api.details(Json::from(json!({ "run": run })), &context).await?;
            if run <= 1 {
                started.resolve();
                cancelled(&context).await;
            }
            Ok(ToolExecutionResult::default())
        }
    });
    (tool, started, runs)
}

#[tokio::test]
async fn answers_an_unsafe_tool_interrupted_after_intent_with_its_durable_partial_output() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let setup = chat_setup(Default::default());
    let (tool, started, runs) = blocking_tool("work");
    add_tool(&setup, tool);
    setup.faux.set_responses([call("work", "c1"), answer("done")]);
    let (harness, root) = open(&path, &setup, None).await;
    let id = submit(&root).await;
    started.wait().await;
    let task_id = tool_task_id(&harness).await;
    harness.close(&ctx()).await.unwrap();

    let (harness, root) = open(&path, &setup, None).await;
    let checkpoint = harness.get_task(task_id, &ctx()).await.unwrap().unwrap().state.checkpoint().map(Value::from);
    let checkpoint = checkpoint.unwrap();
    assert_eq!((checkpoint["phase"].clone(), checkpoint["arguments"].clone()), (json!("execute"), json!({})));
    assert_eq!(checkpoint["replay"], json!("unsafe"));
    assert_eq!(settle(&harness, id).await, SubmissionStatus::Done);
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    let result = results(&all_entries(&root).await).remove(0);
    assert!(result.is_error);
    assert_eq!(result.details, Some(json!({ "run": 1 })));
    assert_eq!(
        text(&result),
        "run 1\n|<harness>\n[error] Tool work was interrupted and may have partially run\n</harness>"
    );
    let live = harness.snapshot(&LIVE_DOC, root.id, &ctx()).await.unwrap();
    assert_eq!(live, Some(Json::from(json!({}))));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn reruns_a_tool_only_when_both_the_stored_and_the_current_replay_policy_are_safe() {
    use ToolReplay::{Safe, Unsafe};
    for (stored, current, reruns) in [(Safe, Safe, true), (Safe, Unsafe, false), (Unsafe, Safe, false)] {
        let directory = tempfile::tempdir().unwrap();
        let path = sqlite_path(&directory);
        let setup = chat_setup(Default::default());
        let (tool, started, runs) = blocking_tool("work");
        add_tool(&setup, tool.clone().with_replay(stored));
        setup.faux.set_responses([call("work", "c1"), answer("done")]);
        let (harness, root) = open(&path, &setup, None).await;
        let id = submit(&root).await;
        started.wait().await;
        harness.close(&ctx()).await.unwrap();

        setup.registry.uninstall("tool:work").unwrap();
        add_tool(&setup, tool.with_replay(current));
        let (harness, root) = open(&path, &setup, None).await;
        assert_eq!(settle(&harness, id).await, SubmissionStatus::Done);
        let result = results(&all_entries(&root).await).remove(0);
        assert_eq!(runs.load(Ordering::SeqCst), if reruns { 2 } else { 1 });
        assert_eq!(result.is_error, !reruns);
        if reruns {
            assert_eq!(text(&result), "run 2\n");
        }
        harness.close(&ctx()).await.unwrap();
    }
}

#[tokio::test]
async fn reruns_a_safe_tool_with_the_environment_of_the_conversation_s_cwd_at_rerun_and_not_once_it_is_deselected() {
    for change in ["cwd", "deselect"] {
        let directory = tempfile::tempdir().unwrap();
        let path = sqlite_path(&directory);
        let setup = chat_setup(Default::default());
        let cwds = Arc::new(Mutex::new(Vec::new()));
        let started = Deferred::default();
        let (sink, signal) = (cwds.clone(), started.clone());
        let work = define_tool("work", "work", no_parameters(), move |_, api, context| {
            let (sink, started) = (sink.clone(), signal.clone());
            async move {
                let count = {
                    let mut cwds = sink.lock().unwrap();
                    cwds.push(api.env.as_ref().map(|env| env.cwd()));
                    cwds.len()
                };
                if count == 1 {
                    started.resolve();
                    cancelled(&context).await;
                }
                Ok(ToolExecutionResult::default())
            }
        })
        .with_replay(ToolReplay::Safe);
        add_tool(&setup, work);
        setup.faux.set_responses([call("work", "c1"), answer("done")]);
        let reopen = || async {
            let options = setup.options().with_env(|target: EnvTarget, _| {
                let cwd = target.cwd.unwrap_or_else(|| "/".into());
                Box::pin(
                    async move { Ok(Some(Arc::new(LocalExecutionEnv::new(LocalEnvOptions::new(cwd))) as SharedEnv)) },
                )
            });
            let opened = open_chat_with(Arc::new(open_sqlite_storage(&path).await.unwrap()), &setup, options).await;
            opened.0.resume().unwrap();
            opened
        };
        let (harness, root) = reopen().await;
        let set_cwd = |cwd: &str| AgentChange { cwd: Change::Set(cwd.into()), ..AgentChange::default() };
        root.configure(set_cwd("/one"), &ctx()).await.unwrap();
        let id = submit(&root).await;
        started.wait().await;
        let next = if change == "cwd" {
            set_cwd("/two")
        } else {
            AgentChange { extensions: Change::Set(ExtensionSelection::List(Vec::new())), ..AgentChange::default() }
        };
        root.configure(next, &ctx()).await.unwrap();
        harness.close(&ctx()).await.unwrap();

        let (harness, root) = reopen().await;
        assert_eq!(settle(&harness, id).await, SubmissionStatus::Done);
        let result = results(&all_entries(&root).await).remove(0);
        let seen: Vec<Option<String>> = cwds.lock().unwrap().clone();
        if change == "cwd" {
            assert_eq!(seen, [Some("/one".to_owned()), Some("/two".to_owned())]);
            assert!(!result.is_error);
        } else {
            // A tool that no longer resolves is treated as unsafe and answered as interrupted.
            assert_eq!(seen, [Some("/one".to_owned())]);
            assert!(text(&result).contains("was interrupted"));
        }
        harness.close(&ctx()).await.unwrap();
    }
}

#[tokio::test]
async fn reruns_before_tool_when_interrupted_before_intent_and_executes_once() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let setup = chat_setup(Default::default());
    let runs = Arc::new(AtomicUsize::new(0));
    let count = runs.clone();
    add_tool(
        &setup,
        define_tool("work", "work", no_parameters(), move |_, _, _| {
            count.fetch_add(1, Ordering::SeqCst);
            async { Ok(ToolExecutionResult { output: Some(Vec::new()), ..Default::default() }) }
        }),
    );
    let reached = Deferred::default();
    let asked = Arc::new(AtomicUsize::new(0));
    let decisions = Arc::new(Mutex::new(Vec::new()));
    let (signal, asks, sink) = (reached.clone(), asked.clone(), decisions.clone());
    let hooks = ToolHooks {
        before_tool: Some(Arc::new(move |_: ToolHookCall, api: HookApi, context: Context| {
            let (reached, sink, ask) = (signal.clone(), sink.clone(), asks.fetch_add(1, Ordering::SeqCst) + 1);
            Box::pin(async move {
                // A durable first-writer-wins decision survives the rerun.
                let decision = api.memo_or("test:decision", Json::from(format!("attempt {ask}")), &context).await?;
                sink.lock().unwrap().push(decision.as_str().unwrap().to_owned());
                if ask == 1 {
                    reached.resolve();
                    cancelled(&context).await;
                }
                Ok(None)
            })
        })),
        after_tool: None,
    };
    setup.registry.install(Extension::new("hooks").hook(hook(&tool_task(), hooks))).unwrap();
    setup.faux.set_responses([call("work", "c1"), answer("done")]);
    let (harness, root) = open(&path, &setup, None).await;
    let id = submit(&root).await;
    reached.wait().await;
    harness.close(&ctx()).await.unwrap();

    let (harness, _) = open(&path, &setup, None).await;
    assert_eq!(settle(&harness, id).await, SubmissionStatus::Done);
    assert_eq!((asked.load(Ordering::SeqCst), runs.load(Ordering::SeqCst)), (2, 1));
    assert_eq!(*decisions.lock().unwrap(), ["attempt 1", "attempt 1"]);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn reruns_the_generation_tools_phase_interrupted_before_its_commit() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let setup = chat_setup(Default::default());
    add_tool(
        &setup,
        define_tool("work", "work", no_parameters(), |_, _, _| async {
            Ok(ToolExecutionResult { output: Some(Vec::new()), ..Default::default() })
        }),
    );
    let reached = Deferred::default();
    let observed = Arc::new(AtomicUsize::new(0));
    let (signal, count) = (reached.clone(), observed.clone());
    let hooks = GenerationHooks {
        after_tools: Some(Arc::new(move |_, _, context: Context| {
            let (reached, seen) = (signal.clone(), count.fetch_add(1, Ordering::SeqCst) + 1);
            Box::pin(async move {
                if seen == 1 {
                    reached.resolve();
                    cancelled(&context).await;
                }
                Ok(())
            })
        })),
        ..GenerationHooks::default()
    };
    setup.registry.install(Extension::new("hooks").hook(hook(&generation_task(), hooks))).unwrap();
    setup.faux.set_responses([call("work", "c1"), answer("done")]);
    let (harness, root) = open(&path, &setup, None).await;
    let id = submit(&root).await;
    reached.wait().await;
    harness.close(&ctx()).await.unwrap();

    let (harness, root) = open(&path, &setup, None).await;
    assert_eq!(settle(&harness, id).await, SubmissionStatus::Done);
    assert_eq!(observed.load(Ordering::SeqCst), 2);
    let expected = ["pi.user", "pi.system", "pi.assistant", "pi.tool-result", "pi.assistant"];
    assert_eq!(kinds(&all_entries(&root).await), expected);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn answers_an_aborted_tool_with_its_partial_output_and_continues_the_run() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let setup = chat_setup(Default::default());
    let (tool, started, _) = blocking_tool("work");
    add_tool(&setup, tool);
    setup.faux.set_responses([call("work", "c1"), answer("done")]);
    let (harness, root) = open(&path, &setup, None).await;
    let submission = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap();
    started.wait().await;
    let task_id = tool_task_id(&harness).await;
    assert_eq!(harness.abort_task(task_id, &ctx()).await.unwrap(), AbortTaskResult::Marked);
    let outcome = serde_json::to_value(harness.wait_for_task(task_id, &ctx()).await.unwrap().state.outcome()).unwrap();
    assert_eq!(outcome["status"], "aborted");
    assert!(outcome["result"]["entryId"].is_u64());
    assert_eq!(submission.wait(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    let result = results(&all_entries(&root).await).remove(0);
    assert_eq!(text(&result), "run 1\n|<harness>\n[error] Tool work was aborted\n</harness>");
    harness.close(&ctx()).await.unwrap();
}

// Not ported: "lets context derivation answer a faulted tool and continues the run". Upstream faults the tool task
// with a result that is not strict JSON (a function in `details`); a Rust `Json` value is always strict JSON.

#[tokio::test]
async fn answers_a_real_bash_command_interrupted_by_close_and_reopen_then_finishes_the_run() {
    if cfg!(windows) {
        // The command is POSIX shell; the Windows shell cases live in the tools tests.
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let env: SharedEnv =
        Arc::new(LocalExecutionEnv::new(LocalEnvOptions::new(directory.path().to_string_lossy().into_owned())));
    let setup = chat_setup(Default::default());
    add_tool(&setup, create_bash_tool(BashToolOptions::default()));
    let command = faux_tool_call_with_id("bash", json!({ "command": "echo started; sleep 30" }), "b");
    let options = FauxMessageOptions::stop_reason(pi_ai::StopReason::ToolUse);
    setup.faux.set_responses([faux_assistant_message(vec![command], options).into(), answer("done")]);
    let (harness, root) = open(&path, &setup, Some(env.clone())).await;
    let id = submit(&root).await;
    wait_for(|| async {
        let live = harness.snapshot(&LIVE_DOC, root.id, &ctx()).await.unwrap().unwrap();
        live["tools"][0]["output"].as_str() == Some("started\n")
    })
    .await;
    harness.close(&ctx()).await.unwrap();

    let (harness, root) = open(&path, &setup, Some(env)).await;
    assert_eq!(settle(&harness, id).await, SubmissionStatus::Done);
    let result = results(&all_entries(&root).await).remove(0);
    assert_eq!(
        text(&result),
        "started\n|<harness>\n[error] Tool bash was interrupted and may have partially run\n</harness>"
    );
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn clears_the_interrupted_attempt_s_progress_before_a_safe_rerun() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let setup = chat_setup(Default::default());
    let started = [Deferred::default(), Deferred::default()];
    let runs = Arc::new(AtomicUsize::new(0));
    let (signals, count) = (started.clone(), runs.clone());
    let info = |message: &str| ToolDiagnostic::new(DiagnosticSeverity::Info, message);
    let work = define_tool("work", "work", no_parameters(), move |_, api, context| {
        let (signals, run) = (signals.clone(), count.fetch_add(1, Ordering::SeqCst));
        async move {
            if run == 0 {
                api.diagnostic(info("first a"))?;
                api.diagnostic(info("first b"))?;
                api.details(Json::from(json!({ "run": 1, "extra": true })), &context).await?;
            } else {
                api.diagnostic(info("second"))?;
                api.details(Json::from(json!({ "run": 2 })), &context).await?;
            }
            signals[run].resolve();
            cancelled(&context).await;
            Ok(ToolExecutionResult::default())
        }
    })
    .with_replay(ToolReplay::Safe);
    add_tool(&setup, work);
    setup.faux.set_responses([call("work", "c1"), answer("done")]);
    let (harness, root) = open(&path, &setup, None).await;
    submit(&root).await;
    started[0].wait().await;
    harness.close(&ctx()).await.unwrap();

    let (harness, root) = open(&path, &setup, None).await;
    started[1].wait().await;
    let live = Value::from(&harness.snapshot(&LIVE_DOC, root.id, &ctx()).await.unwrap().unwrap());
    assert_eq!(live["tools"][0]["diagnostics"], json!([{ "severity": "info", "message": "second" }]));
    assert_eq!(live["tools"][0]["details"], json!({ "run": 2 }));
    harness.close(&ctx()).await.unwrap();
}
