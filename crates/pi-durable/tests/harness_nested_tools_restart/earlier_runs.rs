//! Nested calls an earlier run of a replay-safe caller made, seen by the rerun that makes none.

use std::sync::{Arc, Mutex};

use pi_durable::harness::*;
use pi_durable::{CommitChange, DocumentCommitChange, SubmissionStatus};
use serde_json::Value;

use super::support::chat::{add_tool, answer, call};
use super::support::harness_support::*;
use super::support::{Deferred, commit_observer};
use super::{cancelled, nested_tasks, open, outcome_status, settle, sqlite_path, submit, tool};

/// Counts the runs of a caller, and returns the number of the current one.
fn count_run(runs: &Mutex<u32>) -> u32 {
    let mut runs = runs.lock().unwrap();
    *runs += 1;
    *runs
}

#[tokio::test]
async fn clears_the_slots_of_nested_calls_an_earlier_run_finished_when_the_rerun_makes_none() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let setup = chat_setup(Default::default());
    let (finished, runs) = (Deferred::default(), Arc::new(Mutex::new(0)));
    add_tool(&setup, tool("quick", |_, _, _| async { Ok(ToolExecutionResult::text("ok")) }));
    let (signal, count) = (finished.clone(), runs.clone());
    let caller = tool("caller", move |_, api, context| {
        let (finished, runs) = (signal.clone(), count.clone());
        async move {
            if count_run(&runs) == 1 {
                api.execute_tool("quick", Default::default(), &context, ExecuteToolOptions::default()).await?;
                finished.resolve();
                return cancelled(&context).await;
            }
            Ok(ToolExecutionResult::text("done"))
        }
    });
    add_tool(&setup, caller.with_replay(ToolReplay::Safe));
    setup.faux.set_responses([call("caller", "c1"), answer("done")]);
    let (harness, root) = open(&path, &setup).await;
    let id = submit(&root).await;
    finished.wait().await;
    // The finished nested call's slot is still listed, below its running caller.
    let live = harness.snapshot(&pi_durable::harness::live::LIVE_DOC, root.id, &ctx()).await.unwrap().unwrap();
    assert_eq!(live.to_serde()["nestedTools"].as_array().map(Vec::len), Some(1));
    harness.close(&ctx()).await.unwrap();

    let (harness, _) = open(&path, &setup).await;
    let slots_after_caller = Arc::new(Mutex::new(None));
    let sink = slots_after_caller.clone();
    let _listener = harness.observe_commits(commit_observer(move |publication, _| {
        let ended = publication.changes.iter().any(|change| {
            matches!(change, CommitChange::Task(task)
                if task.kind == "pi.tool" && task.input["kind"] == "model".into() && task.state.outcome().is_some())
        });
        if !ended {
            return Ok(());
        }
        for change in &publication.changes {
            if let CommitChange::Document(DocumentCommitChange::Change { record, value, .. }) = change
                && record.kind == "pi.live"
            {
                let live = Value::from(value.clone());
                *sink.lock().unwrap() = Some(live["nestedTools"].as_array().map_or(0, Vec::len));
            }
        }
        Ok(())
    }));
    harness.resume().unwrap();
    assert_eq!(settle(&harness, id).await, SubmissionStatus::Done);
    assert_eq!(*runs.lock().unwrap(), 2);
    // The caller's terminal commit removed the slot, though the rerun made no nested call.
    assert_eq!(*slots_after_caller.lock().unwrap(), Some(0));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn aborts_a_nested_call_an_earlier_run_of_a_replay_safe_caller_left_running_and_the_rerun_never_made_again() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let setup = chat_setup(Default::default());
    let (running, runs) = (Deferred::default(), Arc::new(Mutex::new(0)));
    let signal = running.clone();
    let slow = tool("slow", move |_, _, context| {
        let running = signal.clone();
        async move {
            running.resolve();
            cancelled(&context).await
        }
    });
    add_tool(&setup, slow.with_replay(ToolReplay::Safe));
    let count = runs.clone();
    let caller = tool("caller", move |_, api, context| {
        let runs = count.clone();
        async move {
            // Only the first run calls `slow`; the rerun after the restart returns at once.
            if count_run(&runs) == 1 {
                let keyed = ExecuteToolOptions { key: Some("slow".into()), ..Default::default() };
                api.execute_tool("slow", Default::default(), &context, keyed).await?;
            }
            Ok(ToolExecutionResult::text("done"))
        }
    });
    add_tool(&setup, caller.with_replay(ToolReplay::Safe));
    setup.faux.set_responses([call("caller", "c1"), answer("done")]);
    let (harness, root) = open(&path, &setup).await;
    let id = submit(&root).await;
    running.wait().await;
    harness.close(&ctx()).await.unwrap();

    let (harness, _) = open(&path, &setup).await;
    harness.resume().unwrap();
    assert_eq!(settle(&harness, id).await, SubmissionStatus::Done);
    assert_eq!(*runs.lock().unwrap(), 2);
    // The settlement of the rerun found the leftover call among the tasks it owns and aborted it.
    let leftover = nested_tasks(&harness).await.remove(0);
    assert_eq!(outcome_status(&leftover), Some("aborted"));
    harness.close(&ctx()).await.unwrap();
}
