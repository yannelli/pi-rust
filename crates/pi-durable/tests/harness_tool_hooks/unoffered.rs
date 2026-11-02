//! Upstream `describe("calls to tools the request did not offer")`: each gets its own tool task (upstream issue 11043).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pi_ai::ContentBlock;
use pi_chord::Context;
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::harness::tool::{ToolHookCall, ToolHooks};
use pi_durable::harness::*;
use pi_durable::storage::MemoryStorage;
use pi_durable::storage::sqlite::open_sqlite_storage;
use pi_durable::{DurableError, ROOT_CONVERSATION_ID, Storage, StorageWrite, SubmissionStatus, TaskId, TaskRecord};
use serde_json::{Value, json};

use super::slot::{open, settle, sqlite_path};
use super::support::Deferred;
use super::support::harness_support::*;
use super::support::task_support::aborted;
use super::{add_hooks, add_tool, calls, diagnostics_of, done, empty, results, submit_and_wait, text, tool};

fn after_tool(
    hook: impl Fn(&ToolHookCall, &ToolExecutionResult, &HookApi) -> Option<ToolExecutionResult> + Send + Sync + 'static,
) -> ToolHooks {
    let after_tool: super::AfterTool =
        Arc::new(move |(call, result): (ToolHookCall, ToolExecutionResult), api: HookApi, _: Context| {
            let replaced = hook(&call, &result, &api);
            Box::pin(async move { Ok(replaced) })
        });
    ToolHooks { after_tool: Some(after_tool), ..ToolHooks::default() }
}

fn sequential(setup: &ChatSetup) {
    setup.settings.lock().unwrap().tool_execution = Some(ToolExecutionMode::Sequential);
}

#[tokio::test]
async fn run_after_tool_inside_their_own_tool_task() {
    let setup = chat_setup(Default::default());
    let seen: Arc<Mutex<Vec<(String, TaskId)>>> = Arc::default();
    let sink = seen.clone();
    add_hooks(
        &setup,
        after_tool(move |call, result, api| {
            sink.lock().unwrap().push((call.call.id.clone(), api.task_id()));
            Some(ToolExecutionResult { output: Some(vec![ContentBlock::text("try another tool")]), ..result.clone() })
        }),
    );
    setup.faux.set_responses([calls(&[("ghost", json!({}), "c1")]), done()]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    submit_and_wait(&root, "go").await;
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(), ["c1"]);
    let task = harness.get_task(seen[0].1, &ctx()).await.unwrap().unwrap();
    assert_eq!(task.kind, "pi.tool");
    let input = task.input.to_serde();
    assert_eq!((&input["kind"], &input["callId"], &input["unoffered"]), (&json!("model"), &json!("c1"), &json!(true)));
    let entries = all_entries(&root).await;
    assert!(text(&results(&entries)["c1"].content).starts_with("try another tool|"));
    assert_eq!(diagnostics_of(&entries, "c1"), ["tool_unavailable"]);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn count_toward_a_round_s_terminate() {
    let setup = chat_setup(Default::default());
    add_tool(&setup, tool("echo", |_, _, _| async { Ok(empty()) }));
    let terminate = ToolControl { terminate: Some(true), ..ToolControl::default() };
    add_hooks(
        &setup,
        after_tool(move |_, result, _| {
            Some(ToolExecutionResult { control: Some(terminate.clone()), ..result.clone() })
        }),
    );
    setup.faux.set_responses([calls(&[("echo", json!({}), "c1"), ("ghost", json!({}), "c2")]), done()]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    assert_eq!(submit_and_wait(&root, "go").await, SubmissionStatus::Done);
    assert_eq!(setup.faux.state().call_count, 1);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn take_their_turn_in_a_sequential_round_and_the_last_handoff_in_call_order_wins() {
    let setup = chat_setup(Default::default());
    sequential(&setup);
    let order: Arc<Mutex<Vec<String>>> = Arc::default();
    for name in ["a", "b"] {
        let order = order.clone();
        add_tool(
            &setup,
            tool(name, move |_, _, _| {
                order.lock().unwrap().push(name.to_owned());
                async { Ok(empty()) }
            }),
        );
    }
    let sink = order.clone();
    add_hooks(
        &setup,
        after_tool(move |call, result, _| {
            let id = &call.call.id;
            sink.lock().unwrap().push(format!("hook {id}"));
            let handoff = ToolControl { handoff: Some(format!("from {id}")), ..ToolControl::default() };
            (id != "c1").then(|| ToolExecutionResult { control: Some(handoff), ..result.clone() })
        }),
    );
    let round = calls(&[("a", json!({}), "c1"), ("ghost", json!({}), "c2"), ("b", json!({}), "c3")]);
    setup.faux.set_responses([round, done()]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    submit_and_wait(&root, "go").await;
    assert_eq!(*order.lock().unwrap(), ["a", "hook c1", "hook c2", "b", "hook c3"]);
    let entries = all_entries(&root).await;
    let resets = entries.iter().filter(|entry| entry.kind == "pi.reset");
    let texts: Vec<String> = resets.filter_map(|entry| text_of(entry.model.as_ref()?.first())).collect();
    assert_eq!(texts, ["from c3"]);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn each_get_a_tool_task_in_a_parallel_round_of_only_such_calls() {
    let setup = chat_setup(Default::default());
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = seen.clone();
    add_hooks(
        &setup,
        after_tool(move |call, _, _| {
            sink.lock().unwrap().push(call.call.id.clone());
            None
        }),
    );
    let round = calls(&[("ghost", json!({}), "c1"), ("spirit", json!({}), "c2"), ("ghost", json!({}), "c3")]);
    setup.faux.set_responses([round, done()]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    assert_eq!(submit_and_wait(&root, "go").await, SubmissionStatus::Done);
    let mut seen = seen.lock().unwrap().clone();
    seen.sort();
    assert_eq!(seen, ["c1", "c2", "c3"]);
    let entries = all_entries(&root).await;
    for id in ["c1", "c2", "c3"] {
        assert_eq!(diagnostics_of(&entries, id), ["tool_unavailable"], "{id}");
    }
    // The round's results go to the next request.
    assert_eq!(setup.faux.state().call_count, 2);
    harness.close(&ctx()).await.unwrap();
}

/// Tool `a`: its first run blocks until its invocation is cancelled, and a safe rerun completes.
fn first_run_blocks(started: &Deferred) -> ToolRegistration {
    let (started, runs) = (started.clone(), Arc::new(AtomicUsize::new(0)));
    tool("a", move |_, _, context: Context| {
        let (started, run) = (started.clone(), runs.fetch_add(1, Ordering::SeqCst) + 1);
        async move {
            if run > 1 {
                return Ok(empty());
            }
            started.resolve();
            aborted(context.abort_signal().unwrap()).await;
            Err(DurableError::error("aborted"))
        }
    })
    .with_replay(ToolReplay::Safe)
}

#[tokio::test]
async fn keep_their_flag_in_a_version_2_generation_s_sequential_round_also_one_migrated_from_version_1() {
    for version in [2, 1] {
        let directory = tempfile::tempdir().unwrap();
        let path = sqlite_path(&directory);
        let setup = chat_setup(Default::default());
        sequential(&setup);
        let started = Deferred::default();
        add_tool(&setup, first_run_blocks(&started));
        add_tool(&setup, tool("ghost", |_, _, _| async { Ok(ToolExecutionResult::text("ran")) }));
        setup.faux.set_responses([calls(&[("a", json!({}), "c1"), ("ghost", json!({}), "c2")]), done()]);
        let (harness, root) = open(&path, &setup).await;
        // `ghost` is registered, but not offered to this request.
        let only_a = AgentChange { tools: Change::Set(ToolFilter::List(vec!["a".into()])), ..AgentChange::default() };
        root.configure(only_a, &ctx()).await.unwrap();
        let id = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().id;
        started.wait().await;
        let live = harness.snapshot(&LIVE_DOC, ROOT_CONVERSATION_ID, &ctx()).await.unwrap().unwrap().to_serde();
        let generation = TaskId(live["run"]["taskId"].as_u64().unwrap());
        let slots = live["tools"].as_array().unwrap().iter();
        let slots: Vec<Value> = slots.map(|slot| json!([slot["callId"], slot["unoffered"]])).collect();
        assert_eq!(slots, [json!(["c1", null]), json!(["c2", true])], "version {version}");
        assert_eq!(harness.get_task(generation, &ctx()).await.unwrap().unwrap().version, 2);
        // The tools are offered when the round goes on: only the flag keeps `ghost` from running.
        root.configure(AgentChange { tools: Change::Clear, ..AgentChange::default() }, &ctx()).await.unwrap();
        harness.close(&ctx()).await.unwrap();
        if version == 1 {
            let storage = open_sqlite_storage(&path).await.unwrap();
            let record = storage.task(generation, &ctx()).await.unwrap().unwrap();
            let old = TaskRecord { version: 1, ..record };
            storage.commit(&[StorageWrite::Task { value: old }], &|_| {}, &ctx()).await.unwrap();
            storage.close(&ctx()).await.unwrap();
        }

        let (harness, root) = open(&path, &setup).await;
        assert_eq!(settle(&harness, id).await, SubmissionStatus::Done, "version {version}");
        assert_eq!(harness.get_task(generation, &ctx()).await.unwrap().unwrap().version, 2);
        let entries = all_entries(&root).await;
        assert_eq!(diagnostics_of(&entries, "c2"), ["tool_unavailable"], "version {version}");
        assert!(!text(&results(&entries)["c2"].content).contains("ran"), "version {version}");
        harness.close(&ctx()).await.unwrap();
    }
}

#[tokio::test]
async fn settle_unavailable_not_aborted_when_their_sequential_round_is_aborted_before_they_start() {
    let setup = chat_setup(Default::default());
    sequential(&setup);
    let started = Deferred::default();
    let signal = started.clone();
    add_tool(
        &setup,
        tool("wait", move |_, _, context: Context| {
            let started = signal.clone();
            async move {
                started.resolve();
                aborted(context.abort_signal().unwrap()).await;
                Err(DurableError::error("aborted"))
            }
        }),
    );
    setup.faux.set_responses([calls(&[("wait", json!({}), "c1"), ("ghost", json!({}), "c2")]), done()]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let submission = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap();
    started.wait().await;
    root.abort(&ctx(), false).await.unwrap();
    assert_eq!(submission.wait(&ctx()).await.unwrap().status, SubmissionStatus::Unanswered);
    let entries = all_entries(&root).await;
    assert_eq!(diagnostics_of(&entries, "c1"), ["aborted"]);
    assert_eq!(diagnostics_of(&entries, "c2"), ["tool_unavailable"]);
    harness.close(&ctx()).await.unwrap();
}
