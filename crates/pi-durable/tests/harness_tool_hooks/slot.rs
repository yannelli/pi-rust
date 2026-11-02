//! Upstream `describe("afterTool on results rebuilt from the slot")`: interruptions, aborts, and abandoned calls.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use pi_ai::ContentBlock;
use pi_chord::{Context, Json};
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::harness::tool::{ToolHookCall, ToolHooks};
use pi_durable::harness::*;
use pi_durable::storage::MemoryStorage;
use pi_durable::storage::sqlite::open_sqlite_storage;
use pi_durable::{DurableError, SubmissionId, SubmissionStatus, TaskId};
use serde_json::json;

use super::nested::batch;
use super::support::Deferred;
use super::support::harness_support::*;
use super::support::task_support::aborted;
use super::{
    AfterTool, Seen, add_hooks, add_tool, calls, diagnostics_of, done, empty, recording, result_codes, results, text,
    tool,
};

/// Open a chat over the SQLite file at `path`, as a fresh process would.
pub async fn open(path: &str, setup: &ChatSetup) -> (Harness, Conversation) {
    let opened = open_chat(Arc::new(open_sqlite_storage(path).await.unwrap()), setup).await;
    opened.0.resume().unwrap();
    opened
}

/// A crash: nothing more reaches Storage, not even the records a close writes; a hanging invocation stays hung.
pub fn kill(harness: &Harness) {
    harness.fail(DurableError::error("crash"), None);
}

pub fn sqlite_path(directory: &tempfile::TempDir) -> String {
    directory.path().join("session.sqlite").to_string_lossy().into_owned()
}

/// Progress commits at once, stored within `persist_within_ms`.
pub fn progress(setup: &ChatSetup, persist_within_ms: f64) {
    let progress = ProgressOverrides {
        output_interval_ms: Some(0),
        persist_within_ms: Some(persist_within_ms),
        ..Default::default()
    };
    setup.settings.lock().unwrap().progress = Some(progress);
}

/// Let deferred progress commits (`persist_within_ms: 0`) reach Storage before a crash.
pub async fn stored() {
    tokio::time::sleep(Duration::from_millis(50)).await;
}

pub async fn settle(harness: &Harness, id: SubmissionId) -> SubmissionStatus {
    harness.submission(id, &ctx()).await.unwrap().unwrap().wait(&ctx()).await.unwrap().status
}

pub async fn tool_task_id(harness: &Harness) -> TaskId {
    let read = || async {
        let live = harness.snapshot(&LIVE_DOC, pi_durable::ROOT_CONVERSATION_ID, &ctx()).await.unwrap();
        live.and_then(|live| live["tools"][0]["taskId"].as_u64()).map(TaskId)
    };
    wait_for(|| async { read().await.is_some() }).await;
    read().await.unwrap()
}

/// A tool that writes output and details, then blocks until its invocation is cancelled.
pub fn blocking(name: &str) -> (ToolRegistration, Deferred, Arc<AtomicUsize>) {
    let (started, runs) = (Deferred::default(), Arc::new(AtomicUsize::new(0)));
    let (signal, count, prefix) = (started.clone(), runs.clone(), name.to_owned());
    let registration = tool(name, move |_, api, context| {
        let (started, run, prefix) = (signal.clone(), count.fetch_add(1, Ordering::SeqCst) + 1, prefix.clone());
        async move {
            api.output(&format!("{prefix} {}\n", "o".repeat(40)))?;
            api.details(Json::from(json!({ "run": run })), &context).await?;
            started.resolve();
            aborted(context.abort_signal().unwrap()).await;
            Err(DurableError::error("aborted"))
        }
    });
    (registration, started, runs)
}

fn output(name: &str) -> String {
    format!("{name} {}", "o".repeat(40))
}

/// The ID and diagnostic codes of each result the hooks saw, sorted.
fn seen_codes(seen: &Seen) -> Vec<(String, Vec<String>)> {
    let mut codes: Vec<_> =
        seen.lock().unwrap().iter().map(|(call, result)| (call.call.id.clone(), result_codes(result))).collect();
    codes.sort();
    codes
}

fn expected(list: &[(&str, &[&str])]) -> Vec<(String, Vec<String>)> {
    list.iter().map(|(id, codes)| ((*id).to_owned(), codes.iter().map(|code| (*code).to_owned()).collect())).collect()
}

#[tokio::test]
async fn runs_on_an_interrupted_call_a_replaced_output_loses_the_slot_s_truncation_diagnostic() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let setup = chat_setup(Default::default());
    progress(&setup, 0.0);
    let limits = ToolOutputLimits { max_bytes: Some(10), retain: Some(Retain::Tail), ..ToolOutputLimits::default() };
    let (work, work_started, _) = blocking("work");
    let (keep, keep_started, _) = blocking("keep");
    add_tool(&setup, work.with_output_limits(limits));
    add_tool(&setup, keep.with_output_limits(limits));
    let seen = Seen::default();
    let after_tool = recording(&seen, |call, result| {
        let redacted = Some(vec![ContentBlock::text("redacted")]);
        (call.call.name == "work").then(|| ToolExecutionResult { output: redacted, ..result.clone() })
    });
    add_hooks(&setup, ToolHooks { after_tool: Some(after_tool), ..ToolHooks::default() });
    setup.faux.set_responses([calls(&[("work", json!({}), "c1"), ("keep", json!({}), "c2")]), done()]);
    let (harness, root) = open(&path, &setup).await;
    let id = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().id;
    work_started.wait().await;
    keep_started.wait().await;
    stored().await;
    kill(&harness);

    let (harness, root) = open(&path, &setup).await;
    assert_eq!(settle(&harness, id).await, SubmissionStatus::Done);
    assert_eq!(seen_codes(&seen), expected(&[("c1", &["interrupted"]), ("c2", &["interrupted"])]));
    let entries = all_entries(&root).await;
    let messages = results(&entries);
    assert!(text(&messages["c1"].content).starts_with("redacted|"));
    assert!(messages["c1"].is_error);
    assert_eq!(messages["c1"].details, Some(json!({ "run": 1 })));
    assert_eq!(diagnostics_of(&entries, "c1"), ["interrupted"]);
    // The kept output keeps its truncation diagnostic, after the hooks' result.
    assert_eq!(diagnostics_of(&entries, "c2"), ["interrupted", "truncated"]);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn runs_on_an_abort_result_but_not_on_the_aborted_results_of_calls_that_never_started() {
    let setup = chat_setup(Default::default());
    setup.settings.lock().unwrap().tool_execution = Some(ToolExecutionMode::Sequential);
    let (first, started, _) = blocking("first");
    add_tool(&setup, first);
    add_tool(&setup, tool("second", |_, _, _| async { Ok(empty()) }));
    let seen = Seen::default();
    let after_tool = recording(&seen, |_, result| {
        Some(ToolExecutionResult { details: Some(Json::from(json!({ "hooked": true }))), ..result.clone() })
    });
    add_hooks(&setup, ToolHooks { after_tool: Some(after_tool), ..ToolHooks::default() });
    setup.faux.set_responses([calls(&[("first", json!({}), "c1"), ("second", json!({}), "c2")]), done()]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let submission = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap();
    started.wait().await;
    root.abort(&ctx(), false).await.unwrap();
    assert_eq!(submission.wait(&ctx()).await.unwrap().status, SubmissionStatus::Unanswered);
    assert_eq!(seen_codes(&seen), expected(&[("c1", &["aborted"])]));
    let messages = results(&all_entries(&root).await);
    assert!(messages["c1"].is_error);
    assert_eq!(messages["c1"].details, Some(json!({ "hooked": true })));
    assert_eq!(messages.get("c2").and_then(|message| message.details.clone()), None);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn reports_a_throwing_hook_on_an_abort_result_and_settles_the_result_as_built() {
    let setup = chat_setup(Default::default());
    let (work, started, _) = blocking("work");
    add_tool(&setup, work);
    let failing: AfterTool = Arc::new(|_: (ToolHookCall, ToolExecutionResult), _: HookApi, _: Context| {
        Box::pin(async { Err(DurableError::error("hook failed")) })
    });
    add_hooks(&setup, ToolHooks { after_tool: Some(failing), ..ToolHooks::default() });
    setup.faux.set_responses([calls(&[("work", json!({}), "c1")]), done()]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let submission = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap();
    started.wait().await;
    root.abort(&ctx(), false).await.unwrap();
    assert_eq!(submission.wait(&ctx()).await.unwrap().status, SubmissionStatus::Unanswered);
    let entries = all_entries(&root).await;
    assert!(text(&results(&entries)["c1"].content).starts_with(&output("work")));
    assert_eq!(diagnostics_of(&entries, "c1"), ["aborted"]);
    assert!(setup.reports().contains(&"hook failed".to_owned()), "{:?}", setup.reports());
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn sees_the_held_progress_of_an_aborted_call_and_the_result_stores_it() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let setup = chat_setup(Default::default());
    // Progress commits are held in memory until the result commit stores them.
    progress(&setup, 60_000.0);
    let (work, started, _) = blocking("work");
    add_tool(&setup, work);
    let seen = Seen::default();
    add_hooks(&setup, ToolHooks { after_tool: Some(recording(&seen, |_, _| None)), ..ToolHooks::default() });
    setup.faux.set_responses([calls(&[("work", json!({}), "c1")]), done()]);
    let (harness, root) = open(&path, &setup).await;
    let submission = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap();
    started.wait().await;
    root.abort(&ctx(), false).await.unwrap();
    assert_eq!(submission.wait(&ctx()).await.unwrap().status, SubmissionStatus::Unanswered);
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].1.details.as_ref().map(Json::to_serde), Some(json!({ "run": 1 })));
    assert_eq!(text(seen[0].1.output.as_deref().unwrap_or_default()), format!("{}\n", output("work")));
    harness.close(&ctx()).await.unwrap();

    let (harness, root) = open(&path, &setup).await;
    let messages = results(&all_entries(&root).await);
    assert!(messages["c1"].is_error);
    assert_eq!(messages["c1"].details, Some(json!({ "run": 1 })));
    assert!(text(&messages["c1"].content).starts_with(&output("work")));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn runs_on_an_abandoned_nested_call_s_result() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let setup = chat_setup(Default::default());
    let (gated, gate) = (Deferred::default(), Arc::new(AtomicBool::new(true)));
    add_tool(&setup, tool("gated", |_, _, _| async { Ok(empty()) }));
    add_tool(&setup, batch(vec![("gated", json!({}))]).0);
    let seen = Seen::default();
    let (signal, holding) = (gated.clone(), gate.clone());
    let hooks = ToolHooks {
        // Holds the nested call in its `call` phase, before intent, until the Harness closes.
        before_tool: Some(Arc::new(move |call: ToolHookCall, _, context: Context| {
            let hold = call.call.name == "gated" && holding.load(Ordering::SeqCst);
            let signal = signal.clone();
            Box::pin(async move {
                if !hold {
                    return Ok(None);
                }
                signal.resolve();
                aborted(context.abort_signal().unwrap()).await;
                Err(DurableError::error("aborted"))
            })
        })),
        after_tool: Some(recording(&seen, |call, result| {
            call.parent.as_ref()?;
            Some(ToolExecutionResult { details: Some(Json::from(json!({ "hooked": true }))), ..result.clone() })
        })),
    };
    add_hooks(&setup, hooks);
    setup.faux.set_responses([calls(&[("batch", json!({}), "c1")]), done()]);
    let (harness, root) = open(&path, &setup).await;
    let id = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().id;
    gated.wait().await;
    harness.close(&ctx()).await.unwrap();
    gate.store(false, Ordering::SeqCst);

    let (harness, _) = open(&path, &setup).await;
    assert_eq!(settle(&harness, id).await, SubmissionStatus::Done);
    seen.lock().unwrap().retain(|(call, _)| call.parent.is_some());
    assert_eq!(seen_codes(&seen), expected(&[("c1/1", &["abandoned"])]));
    harness.close(&ctx()).await.unwrap();
}
