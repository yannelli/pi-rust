//! Upstream `describe("afterTool on results rebuilt from the slot")`: where the restart limit skips the hooks, since
//! they may be what cuts the task off.

use std::sync::Arc;

use pi_chord::Context;
use pi_durable::harness::tool::{ToolHookCall, ToolHooks};
use pi_durable::harness::*;
use pi_durable::{SubmissionStatus, TaskOutcome, TaskState};
use serde_json::json;

use super::slot::{blocking, kill, open, progress, settle, sqlite_path, stored, tool_task_id};
use super::support::Deferred;
use super::support::harness_support::*;
use super::{AfterTool, Seen, add_hooks, add_tool, calls, diagnostics_of, done, result_codes, results, text};

/// An `after_tool` hook that records what it sees and never returns: the host dies inside it.
fn hanging(seen: &Seen, entered: &Deferred) -> AfterTool {
    let (seen, entered) = (seen.clone(), entered.clone());
    Arc::new(move |(call, result): (ToolHookCall, ToolExecutionResult), _: HookApi, _: Context| {
        seen.lock().unwrap().push((call, result));
        entered.resolve();
        Box::pin(std::future::pending())
    })
}

fn seen_codes(seen: &Seen) -> Vec<Vec<String>> {
    seen.lock().unwrap().iter().map(|(_, result)| result_codes(result)).collect()
}

fn restart_limit(setup: &ChatSetup, limit: f64) {
    setup.settings.lock().unwrap().restart_limit = Some(limit);
}

#[tokio::test]
async fn settles_an_abort_without_the_hooks_in_the_abort_handler_s_last_attempt() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let setup = chat_setup(Default::default());
    restart_limit(&setup, 2.0);
    progress(&setup, 0.0);
    let (work, started, _) = blocking("work");
    add_tool(&setup, work);
    let (seen, entered) = (Seen::default(), Deferred::default());
    add_hooks(&setup, ToolHooks { after_tool: Some(hanging(&seen, &entered)), ..ToolHooks::default() });
    setup.faux.set_responses([calls(&[("work", json!({}), "c1")]), done()]);
    let (harness, root) = open(&path, &setup).await;
    let id = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().id;
    started.wait().await;
    let task_id = tool_task_id(&harness).await;
    stored().await;
    let aborting = root.clone();
    drop(tokio::spawn(async move { aborting.abort(&ctx(), false).await }));
    entered.wait().await;
    kill(&harness);

    // One restart, at the limit minus one: the attempt the scheduler would fault when cut off runs no hooks.
    let (harness, root) = open(&path, &setup).await;
    assert_eq!(settle(&harness, id).await, SubmissionStatus::Unanswered);
    assert_eq!(seen_codes(&seen), [["aborted"]]);
    let entries = all_entries(&root).await;
    assert!(text(&results(&entries)["c1"].content).starts_with(&format!("work {}", "o".repeat(40))));
    assert_eq!(diagnostics_of(&entries, "c1"), ["aborted"]);
    let state = harness.get_task(task_id, &ctx()).await.unwrap().unwrap().state;
    assert!(matches!(state, TaskState::Terminal { outcome: TaskOutcome::Aborted { .. } }), "{state:?}");
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn runs_on_a_restart_limit_result_at_the_limit_and_not_once_the_hooks_themselves_were_cut_off() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let setup = chat_setup(Default::default());
    restart_limit(&setup, 1.0);
    progress(&setup, 0.0);
    let (work, started, runs) = blocking("work");
    add_tool(&setup, work);
    let (seen, entered) = (Seen::default(), Deferred::default());
    add_hooks(&setup, ToolHooks { after_tool: Some(hanging(&seen, &entered)), ..ToolHooks::default() });
    setup.faux.set_responses([calls(&[("work", json!({}), "c1")]), done()]);
    let (harness, root) = open(&path, &setup).await;
    let id = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().id;
    started.wait().await;
    let task_id = tool_task_id(&harness).await;
    stored().await;
    kill(&harness);

    // Cut off once, at the limit: the task ends itself, and the hooks run on its result.
    let (harness, _) = open(&path, &setup).await;
    entered.wait().await;
    assert_eq!(seen_codes(&seen), [["restart_limit"]]);
    kill(&harness);

    // Past the limit: ends without the hooks.
    let (harness, root) = open(&path, &setup).await;
    assert_eq!(settle(&harness, id).await, SubmissionStatus::Done);
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(diagnostics_of(&all_entries(&root).await, "c1"), ["restart_limit"]);
    let state = harness.get_task(task_id, &ctx()).await.unwrap().unwrap().state;
    let reason = match &state {
        TaskState::Terminal { outcome: TaskOutcome::Failed { error, .. } } => {
            error.detail.as_ref().map(|detail| detail.to_serde()["reason"].clone())
        }
        _ => None,
    };
    assert_eq!(reason, Some(json!("restart_limit")), "{state:?}");
    harness.close(&ctx()).await.unwrap();
}
