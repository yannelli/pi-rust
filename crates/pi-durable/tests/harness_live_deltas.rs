//! Port of upstream `test/harness-live-deltas.test.ts` (`pi.live` deltas). `harness_live_deltas/streams.rs` holds the
//! partial commits of a streamed answer.

#[path = "harness_live_deltas/streams.rs"]
mod streams;
mod support;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use pi_chord::Json;
use pi_durable::documents::CheckpointInfo;
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::harness::*;
use pi_durable::storage::MemoryStorage;
use pi_durable::types::{CommitChange, CommitPublication, DocumentCommitChange, DocumentContent};
use pi_durable::{EntryRecord, StorageWrite};
use serde_json::{Value, json};
use support::chat::{add_tool, answer, call, calls, no_parameters};
use support::harness_support::*;
use support::{ControlledStorage, commit_observer, document_changes, follow_commits};

type Commits = Arc<Mutex<Vec<Vec<Value>>>>;
type Action = Box<dyn FnOnce(ToolExecutionApi) -> BoxFuture<'static, bool> + Send>;

/// The ops of every `pi.live` commit with operations, as upstream JSON.
fn record_live_ops(harness: &Harness) -> Commits {
    let commits = Commits::default();
    let sink = commits.clone();
    let unsubscribe = follow_commits(harness, move |publication, _| {
        for change in document_changes(publication) {
            if let DocumentCommitChange::Change { record, ops, .. } = change
                && record.kind == "pi.live"
                && !ops.is_empty()
            {
                sink.lock().unwrap().push(ops.iter().map(|op| serde_json::to_value(op).unwrap()).collect());
            }
        }
        Ok(())
    });
    std::mem::forget(unsubscribe);
    commits
}

struct Driven {
    harness: Harness,
    commits: Commits,
    actions: Arc<Mutex<VecDeque<Action>>>,
    wake: Arc<tokio::sync::Notify>,
    submission: Submission,
}

/// Drive one tool call step by step and capture the exact operations of every `pi.live` commit.
async fn drive(limits: ToolOutputLimits) -> Driven {
    let setup = chat_setup(Default::default());
    let actions = Arc::new(Mutex::new(VecDeque::<Action>::new()));
    let wake = Arc::new(tokio::sync::Notify::new());
    let (queue, notify) = (actions.clone(), wake.clone());
    let tool = define_tool("drive", "Driven by the test", no_parameters(), move |_, api, context| {
        let (queue, notify) = (queue.clone(), notify.clone());
        async move {
            loop {
                let next = queue.lock().unwrap().pop_front();
                let Some(action) = next else {
                    let signal = context.abort_signal().unwrap();
                    tokio::select! {
                        () = notify.notified() => continue,
                        error = signal.cancelled() => return Err(error.into()),
                    }
                };
                if !action(api.clone()).await {
                    return Ok(ToolExecutionResult::default());
                }
            }
        }
    })
    .with_output_limits(limits);
    add_tool(&setup, tool);
    setup.faux.set_responses([call("drive", "c1"), answer("done")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let commits = record_live_ops(&harness);
    let submission = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap();
    wait_for(|| async { live(&harness).await["tools"][0]["status"] == "running" }).await;
    Driven { harness, commits, actions, wake, submission }
}

impl Driven {
    fn push(&self, action: Action) {
        self.actions.lock().unwrap().push_back(action);
        self.wake.notify_one();
    }

    /// Run one action inside the tool and return the operations of the commit it caused.
    async fn step(&self, action: impl FnOnce(ToolExecutionApi) -> BoxFuture<'static, ()> + Send + 'static) -> Value {
        let before = self.commits.lock().unwrap().len();
        self.push(Box::new(move |api| {
            Box::pin(async move {
                action(api).await;
                true
            })
        }));
        wait_for(|| async { self.commits.lock().unwrap().len() > before }).await;
        Value::Array(self.commits.lock().unwrap().last().unwrap().clone())
    }

    /// Let the tool return and the run finish; returns the commits made meanwhile.
    async fn finish(&self) -> Vec<Vec<Value>> {
        let before = self.commits.lock().unwrap().len();
        self.push(Box::new(|_| Box::pin(async { false })));
        self.submission.wait(&ctx()).await.unwrap();
        self.commits.lock().unwrap()[before..].to_vec()
    }
}

fn output(text: &'static str) -> impl FnOnce(ToolExecutionApi) -> BoxFuture<'static, ()> + Send {
    move |api| Box::pin(async move { api.output(text).unwrap() })
}

fn owned_output(text: String) -> impl FnOnce(ToolExecutionApi) -> BoxFuture<'static, ()> + Send {
    move |api| Box::pin(async move { api.output(&text).unwrap() })
}

async fn live(harness: &Harness) -> Value {
    let snapshot = harness.snapshot(&LIVE_DOC, pi_durable::ROOT_CONVERSATION_ID, &ctx()).await.unwrap();
    snapshot.map(|value| Value::from(&value)).unwrap_or(Value::Null)
}

fn out() -> Value {
    json!(["tools", 0, "output"])
}

fn contains_all(ops: &[Value], expected: &[Value]) -> bool {
    expected.iter().all(|op| ops.contains(op))
}

#[tokio::test]
async fn hands_a_generation_over_to_its_tool_round_and_starts_a_tool_with_one_field_write_each() {
    let setup = chat_setup(Default::default());
    add_tool(
        &setup,
        define_tool("noop", "noop", no_parameters(), |_, _, _| async {
            Ok(ToolExecutionResult { output: Some(Vec::new()), ..Default::default() })
        }),
    );
    setup.faux.set_responses([call("noop", "c1"), answer("done")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let commits = record_live_ops(&harness);
    root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let tasks = conversation_tasks(&harness, &root).await;
    let ids = |kind: &str| tasks.iter().filter(|task| task.kind == kind).map(|task| task.id.0).collect::<Vec<_>>();
    let mut generations = ids("pi.generation");
    generations.sort();
    let tool = ids("pi.tool")[0];
    let entries: Vec<EntryRecord> = all_entries(&root).await;
    let result = entries.iter().find(|entry| entry.kind == "pi.tool-result").unwrap().id;
    let commits = commits.lock().unwrap().clone();
    assert_eq!(commits.len(), 8, "{commits:?}");
    let run = &commits[0][0];
    assert_eq!(
        (run[0].clone(), run[1].clone(), run[2]["taskId"].clone()),
        (json!("s"), json!(["run"]), json!(generations[0]))
    );
    assert!(run[2]["inputs"][0].is_u64());
    assert_eq!(commits[1], [json!(["s", ["generation"], { "attempt": 1 }])]);
    let pending = json!(["s", ["tools"], [{ "callId": "c1", "name": "noop", "taskId": tool, "status": "pending" }]]);
    assert!(contains_all(&commits[2], &[json!(["d", ["generation"]]), pending]));
    assert_eq!(commits[3], [json!(["s", ["tools", 0, "status"], "running"])]);
    let done = [json!(["s", ["tools", 0, "status"], "done"]), json!(["s", ["tools", 0, "entry"], result])];
    assert!(contains_all(&commits[4], &done));
    let handed = [json!(["d", ["tools"]]), json!(["s", ["run", "taskId"], generations[1]])];
    assert!(contains_all(&commits[5], &handed));
    assert_eq!(commits[6], [json!(["s", ["generation"], { "attempt": 1 }])]);
    assert!(contains_all(&commits[7], &[json!(["d", ["run"]]), json!(["d", ["generation"]])]));
    assert_eq!((commits[2].len(), commits[4].len(), commits[5].len()), (2, 2, 2));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn appends_head_output_and_then_only_updates_the_dropped_counts_once_the_window_is_full() {
    let run = drive(ToolOutputLimits { max_lines: Some(2), ..Default::default() }).await;
    assert_eq!(run.step(output("one\n")).await, json!([["s", out(), "one\n"]]));
    assert_eq!(run.step(output("two\n")).await, json!([["a", out(), "two\n"]]));
    // The window is full: the kept text stays; only the counts change.
    let counts = json!([["s", ["tools", 0, "droppedBytes"], 6], ["s", ["tools", 0, "droppedLines"], 1]]);
    assert_eq!(run.step(output("three\n")).await, counts);
    let counts = json!([["s", ["tools", 0, "droppedBytes"], 11], ["s", ["tools", 0, "droppedLines"], 2]]);
    assert_eq!(run.step(output("four\n")).await, counts);
    run.finish().await;
    run.harness.close(&ctx()).await.unwrap();
}

fn tail(max_lines: usize, max_bytes: Option<usize>) -> ToolOutputLimits {
    ToolOutputLimits { max_lines: Some(max_lines), max_bytes, retain: Some(Retain::Tail) }
}

#[tokio::test]
async fn slides_a_tail_window_as_a_front_trim_plus_an_append() {
    let run = drive(tail(3, None)).await;
    let first = run.step(output("line 1\nline 2\nline 3\n")).await;
    assert_eq!(first, json!([["s", out(), "line 1\nline 2\nline 3\n"]]));
    let expected = json!([
        ["t", out(), 7],
        ["a", out(), "line 4\n"],
        ["s", ["tools", 0, "droppedBytes"], 7],
        ["s", ["tools", 0, "droppedLines"], 1],
    ]);
    assert_eq!(run.step(output("line 4\n")).await, expected);
    // The buffer keeps only the window, and later slides stay minimal and exact.
    let expected = json!([
        ["t", out(), 14],
        ["a", out(), "line 5\nline 6\n"],
        ["s", ["tools", 0, "droppedBytes"], 21],
        ["s", ["tools", 0, "droppedLines"], 3],
    ]);
    assert_eq!(run.step(output("line 5\nline 6\n")).await, expected);
    assert_eq!(live(&run.harness).await["tools"][0]["output"], "line 4\nline 5\nline 6\n");
    run.finish().await;
    run.harness.close(&ctx()).await.unwrap();
}

fn output_ops(ops: &Value) -> Vec<Value> {
    ops.as_array().unwrap().iter().filter(|op| op[1] == out()).cloned().collect()
}

#[tokio::test]
async fn writes_the_whole_window_when_chord_s_overlap_search_cannot_find_the_shared_part() {
    // A kept window beyond the 64 KiB overlap scan.
    let wide = drive(tail(1_000_000, Some(100 * 1024))).await;
    let line = |index: usize| format!("{index:010} {}\n", "x".repeat(989));
    let text: String = (0..100).map(line).collect();
    wide.step(owned_output(text)).await;
    let slid = wide.step(owned_output((100..104).map(line).collect())).await;
    let verbs: Vec<Value> = output_ops(&slid).iter().map(|op| op[0].clone()).collect();
    assert_eq!(verbs, [json!("s")]);
    wide.finish().await;
    wide.harness.close(&ctx()).await.unwrap();

    // Repetitive output still finds an overlap here; Chord's bounded candidate search can give up on other inputs
    // and then writes one window.
    let repetitive = drive(tail(50, None)).await;
    repetitive.step(owned_output("y\n".repeat(50))).await;
    let repeated = repetitive.step(output("z\n")).await;
    assert_eq!(output_ops(&repeated), [json!(["t", out(), 2]), json!(["a", out(), "z\n"])]);
    repetitive.finish().await;
    repetitive.harness.close(&ctx()).await.unwrap();
}

fn details(value: Value) -> impl FnOnce(ToolExecutionApi) -> BoxFuture<'static, ()> + Send {
    move |api| Box::pin(async move { api.details(Json::from(value), &ctx()).await.unwrap() })
}

fn diagnostic(
    severity: DiagnosticSeverity,
    message: &'static str,
) -> impl FnOnce(ToolExecutionApi) -> BoxFuture<'static, ()> + Send {
    move |api| Box::pin(async move { api.diagnostic(ToolDiagnostic::new(severity, message)).unwrap() })
}

#[tokio::test]
async fn diffs_details_leaf_by_leaf_and_appends_diagnostics() {
    let run = drive(ToolOutputLimits::default()).await;
    let path = json!(["tools", 0, "details"]);
    assert_eq!(
        run.step(details(json!({ "step": 1, "log": "a" }))).await,
        json!([["s", path, { "step": 1, "log": "a" }]])
    );
    let ops = run.step(details(json!({ "step": 2, "log": "ab" }))).await;
    let expected =
        [json!(["s", ["tools", 0, "details", "step"], 2]), json!(["a", ["tools", 0, "details", "log"], "b"])];
    assert!(contains_all(ops.as_array().unwrap(), &expected), "{ops}");
    assert_eq!(run.step(details(json!({ "step": 2 }))).await, json!([["d", ["tools", 0, "details", "log"]]]));
    let diagnostics = json!(["tools", 0, "diagnostics"]);
    let first = json!({ "severity": "info", "message": "first" });
    let second = json!({ "severity": "warn", "message": "second" });
    assert_eq!(run.step(diagnostic(DiagnosticSeverity::Info, "first")).await, json!([["s", diagnostics, [first]]]));
    assert_eq!(
        run.step(diagnostic(DiagnosticSeverity::Warn, "second")).await,
        json!([["p", diagnostics, 1, 0, [second]]])
    );
    // Settlement moves everything into the result entry and keeps the slot small.
    let settled = run.finish().await.remove(0);
    let expected = [json!(["s", ["tools", 0, "status"], "done"]), json!(["d", path]), json!(["d", diagnostics])];
    assert!(contains_all(&settled, &expected), "{settled:?}");
    run.harness.close(&ctx()).await.unwrap();
}

fn nothing_runs(value: &Value) -> bool {
    value.get("generation").is_none()
        && !value["tools"].as_array().is_some_and(|slots| slots.iter().any(|slot| slot["status"] == "running"))
}

#[tokio::test]
async fn stores_a_complete_base_exactly_in_the_commits_where_nothing_runs() {
    let setup = chat_setup(Default::default());
    for name in ["first", "second"] {
        add_tool(
            &setup,
            define_tool(name, name, no_parameters(), move |_, api, _| async move {
                api.output(&format!("{name} output\n"))?;
                tokio::time::sleep(Duration::from_millis(150)).await;
                api.output(&format!("{name} more\n"))?;
                Ok(ToolExecutionResult::default())
            }),
        );
    }
    setup.faux.set_responses([calls(&[("first", json!({}), "a"), ("second", json!({}), "b")]), answer("done")]);
    setup.settings.lock().unwrap().tool_execution = Some(ToolExecutionMode::Sequential);
    let storage = Arc::new(ControlledStorage::default());
    let (harness, root) = open_chat(storage.clone(), &setup).await;
    // Per publication, in order: the pi.live document's ID and value when the commit changed it.
    let published = Arc::new(Mutex::new(Vec::new()));
    let sink = published.clone();
    let unsubscribe = harness.observe_commits(commit_observer(move |publication: &CommitPublication, _| {
        let live = publication.changes.iter().find_map(|change| match change {
            CommitChange::Document(DocumentCommitChange::Change { record, value, .. }) if record.kind == "pi.live" => {
                Some((record.id, Value::from(value)))
            }
            _ => None,
        });
        sink.lock().unwrap().push(live);
        Ok(())
    }));
    std::mem::forget(unsubscribe);
    let first = storage.commit_count();
    root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    // Each admitted batch has one publication, in the same order.
    let published = published.lock().unwrap().clone();
    let mut kinds = Vec::new();
    for (writes, live) in storage.commits()[first..].iter().zip(&published) {
        let Some((id, value)) = live else { continue };
        let written = writes.iter().find_map(|write| match write {
            StorageWrite::DocumentChange { id: changed, content } if changed == id => {
                Some(matches!(content, DocumentContent::Base { .. }))
            }
            _ => None,
        });
        let Some(base) = written else { continue };
        assert_eq!(base, nothing_runs(value), "{value}");
        kinds.push(base);
    }
    // Bases at the handover, after each sequential tool, after the round, and when the run ends.
    assert!(kinds.iter().filter(|base| **base).count() >= 5, "{kinds:?}");
    assert!(kinds.contains(&false));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn starts_calls_the_request_did_not_offer_as_done() {
    let setup = chat_setup(Default::default());
    add_tool(
        &setup,
        define_tool("bad", "bad", no_parameters(), |_, _, _| async {
            Ok(ToolExecutionResult { output: Some(Vec::new()), ..Default::default() })
        }),
    );
    setup.faux.set_responses([calls(&[("ghost", json!({}), "g"), ("bad", json!({}), "b")]), answer("done")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let commits = record_live_ops(&harness);
    root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let entries = all_entries(&root).await;
    let ghost = entries.iter().find(|entry| entry.kind == "pi.tool-result").unwrap().id;
    let commits = commits.lock().unwrap().clone();
    let handover = commits.iter().find(|ops| ops.iter().any(|op| op[0] == "s" && op[1][0] == "tools")).unwrap();
    let slots = handover.iter().find(|op| op[1] == json!(["tools"])).unwrap();
    assert_eq!(slots[2][0], json!({ "callId": "g", "name": "ghost", "status": "done", "entry": ghost }));
    assert_eq!((slots[2][1]["callId"].clone(), slots[2][1]["status"].clone()), (json!("b"), json!("pending")));
    assert!(slots[2][1]["taskId"].is_u64());
    harness.close(&ctx()).await.unwrap();
}

// Not ported: the second half of "starts calls the request did not offer as done and marks a faulted tool's slot
// done without an entry". Upstream faults the tool with a result that is not strict JSON; a Rust `Json` value is
// always strict JSON, so the fault cleanup cannot be reached this way.

#[tokio::test]
async fn commits_the_tool_calling_answer_its_tool_tasks_the_generation_s_wait_and_the_tool_round_in_one_commit() {
    let setup = chat_setup(Default::default());
    add_tool(
        &setup,
        define_tool("noop", "noop", no_parameters(), |_, _, _| async {
            Ok(ToolExecutionResult { output: Some(Vec::new()), ..Default::default() })
        }),
    );
    setup.faux.set_responses([calls(&[("noop", json!({}), "a"), ("noop", json!({}), "b")]), answer("done")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let handover = Arc::new(Mutex::new(None::<CommitPublication>));
    let sink = handover.clone();
    let unsubscribe = follow_commits(&harness, move |publication, _| {
        for change in document_changes(publication) {
            if let DocumentCommitChange::Change { record, value, .. } = change
                && record.kind == "pi.live"
                && value["tools"].as_array().is_some_and(|tools| tools.len() == 2)
            {
                sink.lock().unwrap().get_or_insert_with(|| publication.clone());
            }
        }
        Ok(())
    });
    std::mem::forget(unsubscribe);
    root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let publication = handover.lock().unwrap().clone().unwrap();
    let mut kinds: Vec<String> = publication
        .changes
        .iter()
        .filter_map(|change| match change {
            CommitChange::Entry(entry) => Some(entry.kind.clone()),
            CommitChange::Task(task) => Some(task.kind.clone()),
            _ => None,
        })
        .collect();
    kinds.sort();
    assert_eq!(kinds, ["pi.assistant", "pi.generation", "pi.tool", "pi.tool"]);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn writes_an_aborted_tool_s_slot_with_field_level_ops() {
    let run = drive(ToolOutputLimits::default()).await;
    run.step(output("partial\n")).await;
    run.step(details(json!({ "n": 1 }))).await;
    let task_id = pi_durable::TaskId(live(&run.harness).await["tools"][0]["taskId"].as_u64().unwrap());
    let before = run.commits.lock().unwrap().len();
    run.harness.abort_task(task_id, &ctx()).await.unwrap();
    let is_abort = |ops: &Vec<Value>| ops.iter().any(|op| op[0] == "s" && op[1] == json!(["tools", 0, "status"]));
    wait_for(|| async { run.commits.lock().unwrap()[before..].iter().any(is_abort) }).await;
    let abort = run.commits.lock().unwrap()[before..].iter().find(|ops| is_abort(ops)).cloned().unwrap();
    assert!(contains_all(
        &abort,
        &[json!(["s", ["tools", 0, "status"], "done"]), json!(["d", out()]), json!(["d", ["tools", 0, "details"]])]
    ));
    assert!(abort.iter().any(|op| op[0] == "s" && op[1] == json!(["tools", 0, "entry"]) && op[2].is_u64()));
    assert_eq!(abort.len(), 4, "{abort:?}");
    run.harness.close(&ctx()).await.unwrap();
}

#[test]
fn keeps_a_complete_base_exactly_while_nothing_runs() {
    let base = |value: Value| {
        let info = CheckpointInfo { deltas_since_base: 1000 };
        LIVE_DOC.definition().checkpoint_when(&Json::from(value), &[], info).unwrap()
    };
    let slot = |status: &str| json!({ "callId": "c", "name": "n", "status": status });
    let run = json!({ "taskId": 1, "inputs": [] });
    assert!(base(json!({})));
    assert!(!base(json!({ "run": run, "generation": { "attempt": 1 } })));
    assert!(base(json!({ "run": run, "tools": [slot("pending"), slot("done")] })));
    assert!(!base(json!({ "run": run, "tools": [slot("done"), slot("running")] })));
    assert!(base(json!({ "run": run })));
}
