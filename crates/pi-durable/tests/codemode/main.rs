//! Port of upstream `test/codemode.test.ts` (durable code mode). This file holds the helpers and the cases without a
//! restart; `restarts.rs` holds the resume cases and `divergence.rs` the cases where a rerun takes another path.

#[path = "../support/mod.rs"]
mod support;

mod bench;
mod describe;
mod divergence;
mod env;
mod models;
mod restarts;
mod results;

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_ai::{ContentBlock, Message};
use pi_chord::{Context, Json};
use pi_codemode::js::js_stringify;
use pi_codemode::{CodemodeRun, CodemodeRunOptions, CodemodeSandbox, SandboxError, SandboxOptions};
use pi_durable::codemode::{
    CodemodeGlobal, CodemodeNamespace, CodemodeNestedCallStatus, CodemodeSandboxBase, CodemodeToolDetails,
    CodemodeToolOptions, create_codemode_tool,
};
use pi_durable::harness::*;
use pi_durable::storage::MemoryStorage;
use pi_durable::storage::sqlite::open_sqlite_storage;
use pi_durable::testing::faux::FauxResponseStep;
use pi_durable::types::DocumentCommitChange;
use pi_durable::*;
use serde_json::{Value, json};
use support::chat::{add_tool, answer, calls};
use support::harness_support::*;
use support::task_support::{aborted, eventually};
use support::{ControlledStorage, Deferred, document_changes};

fn parameters() -> Value {
    json!({ "type": "object", "properties": { "text": { "type": "string" }, "id": { "type": "number" } } })
}

pub fn tool<F, Fut>(name: &str, execute: F) -> ToolRegistration
where
    F: Fn(pi_ai::JsonObject, ToolExecutionApi, Context) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = DurableResult<ToolExecutionResult>> + Send + 'static,
{
    define_tool(name, name, parameters(), execute)
}

/// `args.<key>` as JavaScript prints it; empty when absent.
fn arg(args: &pi_ai::JsonObject, key: &str) -> String {
    match args.get(key) {
        Some(Value::String(text)) => text.clone(),
        Some(value) => js_stringify(value),
        None => String::new(),
    }
}

pub fn done() -> FauxResponseStep {
    answer("done")
}

pub fn script(code: &str) -> FauxResponseStep {
    calls(&[("codemode", json!({ "code": code }), "s1")])
}

/// Counts VM starts; `on_start` sees each run.
#[derive(Default)]
pub struct Vm {
    pub starts: AtomicUsize,
    #[allow(clippy::type_complexity)]
    pub on_start: Mutex<Option<Box<dyn Fn(&mut CodemodeRun) + Send + Sync>>>,
}

struct CountingSandbox {
    inner: Arc<CodemodeSandbox>,
    vm: Arc<Vm>,
}

impl CodemodeSandboxBase for CountingSandbox {
    fn start(&self, code: &str, options: CodemodeRunOptions) -> Result<CodemodeRun, SandboxError> {
        self.vm.starts.fetch_add(1, Ordering::SeqCst);
        let mut run = self.inner.start(code, options)?;
        if let Some(hook) = &*self.vm.on_start.lock().unwrap() {
            hook(&mut run);
        }
        Ok(run)
    }
}

/// `hold` waits for an abort while `block` is set, after resolving `reached`.
pub struct Holds {
    pub block: AtomicBool,
    pub reached: Mutex<Deferred>,
    pub runs: AtomicUsize,
}

impl Holds {
    pub async fn reached(&self) {
        let reached = self.reached.lock().unwrap().clone();
        reached.wait().await;
    }
}

/// `slow` returns once `release(id)` was called.
#[derive(Default)]
pub struct Slows {
    gates: Mutex<HashMap<u64, Deferred>>,
    pub finished: Mutex<Vec<u64>>,
}

impl Slows {
    fn gate(&self, id: u64) -> Deferred {
        self.gates.lock().unwrap().entry(id).or_default().clone()
    }

    pub fn release(&self, id: u64) {
        self.gate(id).resolve();
    }
}

/// A chat with the code mode tool and test tools: `echo` returns `echo <text>` and counts its runs by text; `hold`
/// (replay-safe) waits while `holds.block` is set, then returns `held <id>`; `slow` returns `slow <id>` once released.
pub struct Chat {
    pub setup: ChatSetup,
    pub sandbox: Arc<CodemodeSandbox>,
    pub vm: Arc<Vm>,
    pub echo: Arc<Mutex<BTreeMap<String, usize>>>,
    pub holds: Arc<Holds>,
    pub slows: Arc<Slows>,
}

pub fn new_sandbox() -> Arc<CodemodeSandbox> {
    let options = SandboxOptions { memory_limit_bytes: Some(64 * 1024 * 1024), ..SandboxOptions::default() };
    Arc::new(CodemodeSandbox::new(options).unwrap())
}

pub fn codemode_chat(globals: Vec<CodemodeGlobal>) -> Chat {
    codemode_chat_with(globals, Vec::new())
}

pub fn codemode_chat_with(globals: Vec<CodemodeGlobal>, namespaces: Vec<CodemodeNamespace>) -> Chat {
    let setup = chat_setup(Default::default());
    let vm = Arc::new(Vm::default());
    let sandbox = new_sandbox();
    let mut options = CodemodeToolOptions::new(Arc::new(CountingSandbox { inner: sandbox.clone(), vm: vm.clone() }));
    (options.globals, options.namespaces) = (globals, namespaces);
    add_tool(&setup, create_codemode_tool(options));
    let echo = Arc::new(Mutex::new(BTreeMap::new()));
    let counts = echo.clone();
    add_tool(
        &setup,
        tool("echo", move |args, _, _| {
            let text = arg(&args, "text");
            *counts.lock().unwrap().entry(text.clone()).or_insert(0) += 1;
            async move { Ok(ToolExecutionResult::text(format!("echo {text}"))) }
        }),
    );
    let holds = Arc::new(Holds { block: AtomicBool::new(true), reached: Mutex::default(), runs: AtomicUsize::new(0) });
    let state = holds.clone();
    let hold = tool("hold", move |args, _, context| {
        let holds = state.clone();
        async move {
            holds.runs.fetch_add(1, Ordering::SeqCst);
            if holds.block.load(Ordering::SeqCst) {
                holds.reached.lock().unwrap().resolve();
                aborted(context.abort_signal().unwrap()).await;
            }
            Ok(ToolExecutionResult::text(format!("held {}", arg(&args, "id"))))
        }
    });
    add_tool(&setup, hold.with_replay(ToolReplay::Safe));
    let slows = Arc::new(Slows::default());
    let state = slows.clone();
    add_tool(
        &setup,
        tool("slow", move |args, _, _| {
            let slows = state.clone();
            async move {
                let id = args.get("id").and_then(Value::as_u64).unwrap_or(0);
                slows.gate(id).wait().await;
                slows.finished.lock().unwrap().push(id);
                Ok(ToolExecutionResult::text(format!("slow {id}")))
            }
        }),
    );
    Chat { setup, sandbox, vm, echo, holds, slows }
}

pub fn echoes(chat: &Chat) -> BTreeMap<String, usize> {
    chat.echo.lock().unwrap().clone()
}

pub fn sqlite_path(directory: &tempfile::TempDir) -> String {
    directory.path().join("session.sqlite").to_string_lossy().into_owned()
}

pub async fn open(path: &str, setup: &ChatSetup) -> (Harness, Conversation) {
    open_chat(Arc::new(open_sqlite_storage(path).await.unwrap()), setup).await
}

async fn tool_tasks(harness: &Harness) -> Vec<TaskRecord> {
    let query = TaskQuery { kind: Some("pi.tool".into()), ..TaskQuery::default() };
    harness.commit(move |tx| async move { tx.scan_tasks(query, 100, None).await }, &ctx()).await.unwrap().items
}

pub async fn script_task(harness: &Harness) -> TaskId {
    let tasks = tool_tasks(harness).await;
    tasks.into_iter().find(|task| task.input["kind"] == Json::from("model")).unwrap().id
}

pub async fn nested_tasks(harness: &Harness) -> Vec<TaskRecord> {
    let tasks = tool_tasks(harness).await;
    tasks.into_iter().filter(|task| task.input["kind"] == Json::from("nested")).collect()
}

pub async fn nested_count(harness: &Harness) -> usize {
    nested_tasks(harness).await.len()
}

/// The result's text without the "Script completed" header and its wall time, which tests of it check themselves.
pub fn without_header(text: &str) -> String {
    for head in ["Script completed\nWall time ", "Script failed\nWall time "] {
        if let Some(rest) = text.strip_prefix(head)
            && let Some((seconds, rest)) = rest.split_once(" seconds\nOutput:\n")
            && seconds.chars().all(|c| c.is_ascii_digit() || c == '.')
        {
            return rest.to_owned();
        }
    }
    text.to_owned()
}

fn text_and_error(entry: &EntryRecord) -> (String, bool) {
    match entry.model.as_ref().and_then(|model| model.first()) {
        Some(Message::ToolResult(result)) => {
            let texts = result.content.iter().map(|item| match item {
                ContentBlock::Text { text, .. } => text.as_str(),
                _ => "",
            });
            (without_header(&texts.collect::<String>()), result.is_error)
        }
        _ => (String::new(), false),
    }
}

/// The code mode call's result text, and whether it is an error.
pub async fn result_text(root: &Conversation) -> (String, bool) {
    let entries = all_entries(root).await;
    text_and_error(entries.iter().find(|entry| entry.kind == "pi.tool-result").unwrap())
}

pub async fn wait(harness: &Harness, id: SubmissionId) -> SubmissionStatus {
    harness.submission(id, &ctx()).await.unwrap().unwrap().wait(&ctx()).await.unwrap().status
}

pub async fn submit(root: &Conversation, text: &str) -> SubmissionId {
    root.submit(SubmissionDraft::input(text), &ctx()).await.unwrap().id
}

/// Open over a fresh SQLite file and run the queued responses to the end.
pub async fn run_once(chat: &Chat, directory: &tempfile::TempDir) -> (Harness, Conversation) {
    let (harness, root) = open(&sqlite_path(directory), &chat.setup).await;
    harness.resume().unwrap();
    let id = submit(&root, "go").await;
    assert_eq!(wait(&harness, id).await, SubmissionStatus::Done);
    (harness, root)
}

#[tokio::test]
async fn runs_a_script_that_calls_tools_as_nested_calls_and_journals_each_result_before_delivering_it() {
    let chat = codemode_chat(vec![]);
    chat.holds.block.store(false, Ordering::SeqCst);
    let code =
        r#"const a = await tools.echo({ text: "a" }); const b = await tools.echo({ text: "b" }); return [a, b];"#;
    chat.setup.faux.set_responses([script(code), done()]);
    let directory = tempfile::tempdir().unwrap();
    let (harness, root) = run_once(&chat, &directory).await;
    assert_eq!(result_text(&root).await, (r#"["echo a","echo b"]"#.into(), false));
    assert_eq!(nested_count(&harness).await, 2);
    harness.close(&ctx()).await.unwrap();
}

fn fails_tool() -> ToolRegistration {
    tool("fails", |_, _, _| async {
        Ok(ToolExecutionResult { is_error: Some(true), ..ToolExecutionResult::text("permission denied") })
    })
}

#[tokio::test]
async fn lists_the_script_s_tool_calls_in_its_result_s_details() {
    let chat = codemode_chat(vec![]);
    add_tool(&chat.setup, fails_tool());
    let code =
        r#"await tools.echo({ text: "a" }); try { await tools.fails({}); } catch {} tools.hold({ id: 9 }); return 1;"#;
    chat.setup.faux.set_responses([script(code), done()]);
    let directory = tempfile::tempdir().unwrap();
    let (harness, root) = run_once(&chat, &directory).await;
    let entries = all_entries(&root).await;
    let entry = entries.iter().find(|entry| entry.kind == "pi.tool-result").unwrap();
    let Some(Message::ToolResult(result)) = entry.model.as_ref().and_then(|model| model.first()) else { panic!() };
    let details: CodemodeToolDetails = serde_json::from_value(result.details.clone().unwrap()).unwrap();
    let rows: Vec<_> = details
        .calls
        .iter()
        .map(|call| (call.id, call.name.as_str(), call.args.as_str(), call.status, call.error.as_deref()))
        .collect();
    // A call still running, or not yet launched, when the script ended shows as cancelled.
    assert_eq!(
        rows,
        [
            (1, "echo", r#"{"text":"a"}"#, CodemodeNestedCallStatus::Ok, None),
            (2, "fails", "{}", CodemodeNestedCallStatus::Error, Some("permission denied")),
            (3, "hold", r#"{"id":9}"#, CodemodeNestedCallStatus::Cancelled, None),
        ]
    );
    assert!(details.calls[0].duration_ms.is_some());
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn aborts_a_call_still_running_when_the_script_ends() {
    let chat = codemode_chat(vec![]);
    chat.setup.faux.set_responses([script(r#"tools.hold({}); await tools.echo({ text: "a" }); return 1;"#), done()]);
    let directory = tempfile::tempdir().unwrap();
    let (harness, root) = run_once(&chat, &directory).await;
    assert_eq!(result_text(&root).await, ("1".into(), false));
    let tasks = nested_tasks(&harness).await;
    let hold = tasks.iter().find(|task| task.input["call"]["name"] == Json::from("hold")).unwrap();
    assert_eq!(chat.holds.runs.load(Ordering::SeqCst), 1);
    let outcome = match &hold.state {
        TaskState::Terminal { outcome } => Some(outcome.status()),
        _ => None,
    };
    assert_eq!(outcome, Some("aborted"));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn admits_no_call_once_the_script_has_ended_even_one_it_made_before_ending() {
    let chat = codemode_chat(vec![]);
    chat.holds.block.store(false, Ordering::SeqCst);
    let storage = Arc::new(ControlledStorage::default());
    // Hold the fingerprint commit, and so the admission of the script's call, until the script has ended.
    let held_storage = storage.clone();
    *chat.vm.on_start.lock().unwrap() = Some(Box::new(move |run: &mut CodemodeRun| {
        let held = held_storage.hold_commits();
        let result = std::mem::replace(&mut run.result, Box::pin(std::future::pending()));
        run.result = Box::pin(async move {
            let result = result.await;
            held.release();
            result
        });
    }));
    chat.setup.faux.set_responses([script(r#"tools.echo({ text: "late" }); return 1;"#), done()]);
    let (harness, root) = open_chat(storage, &chat.setup).await;
    harness.resume().unwrap();
    let id = submit(&root, "go").await;
    assert_eq!(wait(&harness, id).await, SubmissionStatus::Done);
    assert_eq!(result_text(&root).await, ("1".into(), false));
    assert!(echoes(&chat).is_empty());
    assert_eq!(nested_count(&harness).await, 0);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn rejects_a_failed_tool_call_with_its_error_text_and_structured_output() {
    let chat = codemode_chat(vec![]);
    add_tool(&chat.setup, fails_tool());
    chat.holds.block.store(false, Ordering::SeqCst);
    let code = "try { await tools.fails({}); } catch (e) { return [e.message, e.structuredOutput]; }";
    chat.setup.faux.set_responses([script(code), done()]);
    let directory = tempfile::tempdir().unwrap();
    let (harness, root) = run_once(&chat, &directory).await;
    assert_eq!(result_text(&root).await, (r#"["permission denied","permission denied"]"#.into(), false));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn delivers_a_result_only_after_its_journal_commit() {
    let storage = Arc::new(ControlledStorage::default());
    let seen = Arc::new(Mutex::new(Vec::<&str>::new()));
    let (first_seen, first_storage, second_seen) = (seen.clone(), storage.clone(), seen.clone());
    let first = CodemodeGlobal::new("first", move |_, _, _| {
        first_seen.lock().unwrap().push("first");
        // The next commit is this result's journal commit: it never lands.
        first_storage.fail_next_commit(DurableError::error("lost"));
        async { Ok(Some(json!(1))) }
    });
    let second = CodemodeGlobal::new("second", move |_, _, _| {
        second_seen.lock().unwrap().push("second");
        async { Ok(Some(json!(2))) }
    });
    let chat = codemode_chat(vec![first, second]);
    chat.holds.block.store(false, Ordering::SeqCst);
    chat.setup.faux.set_responses([script("await first(); await second();"), done()]);
    let (harness, root) = open_chat(storage, &chat.setup).await;
    harness.resume().unwrap();
    submit(&root, "go").await;
    eventually(|| async { seen.lock().unwrap().contains(&"first") }).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(*seen.lock().unwrap(), ["first"]);
    let _ = harness.close(&ctx()).await;
}

#[tokio::test]
async fn keeps_store_values_per_conversation_as_of_a_fork_s_entry() {
    let chat = codemode_chat(vec![]);
    chat.holds.block.store(false, Ordering::SeqCst);
    let read = r#"return [load("k") ?? null]"#;
    chat.setup.faux.set_responses([script(r#"store("k", 1); return load("k")"#), done(), script(read), done()]);
    let directory = tempfile::tempdir().unwrap();
    let (harness, root) = open(&sqlite_path(&directory), &chat.setup).await;
    harness.resume().unwrap();
    let id = submit(&root, "first").await;
    wait(&harness, id).await;
    let first_user = all_entries(&root).await.into_iter().find(|entry| entry.kind == "pi.user").unwrap();
    let id = submit(&root, "second").await;
    wait(&harness, id).await;
    let entries = all_entries(&root).await;
    let results = entries.iter().filter(|entry| entry.kind == "pi.tool-result");
    assert_eq!(results.map(|entry| text_and_error(entry).0).collect::<Vec<_>>(), ["1", "[1]"]);
    // A fork at the first input is from before the first script stored anything.
    chat.setup.faux.set_responses([script(read), done()]);
    let fork = root.fork(first_user.id, CreateOptions::default(), &ctx()).await.unwrap();
    let id = submit(&fork, "fork").await;
    wait(&harness, id).await;
    let entries = all_entries(&fork).await;
    let last = entries.iter().rfind(|entry| entry.kind == "pi.tool-result").unwrap();
    assert_eq!(text_and_error(last).0, "[null]");
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn journals_results_that_finish_together_in_one_commit() {
    let (release, waiting) = (Deferred::default(), Arc::new(AtomicUsize::new(0)));
    let gate = release.clone();
    let wait_global = CodemodeGlobal::new("wait", move |_, _, _| {
        let release = gate.clone();
        if waiting.fetch_add(1, Ordering::SeqCst) + 1 == 50 {
            release.resolve();
        }
        async move {
            release.wait().await;
            Ok(Some(Value::Null))
        }
    });
    let chat = codemode_chat(vec![wait_global]);
    chat.holds.block.store(false, Ordering::SeqCst);
    let code = "return (await Promise.all(Array.from({ length: 50 }, () => wait()))).length";
    chat.setup.faux.set_responses([script(code), done()]);
    let directory = tempfile::tempdir().unwrap();
    let (harness, root) = open(&sqlite_path(&directory), &chat.setup).await;
    let commits = Arc::new(AtomicUsize::new(0));
    let counter = commits.clone();
    let listener = harness.subscribe_commits(move |publication, _| {
        for change in document_changes(publication) {
            if let DocumentCommitChange::Document { record, .. } = change
                && record.kind == "pi.codemode.journal"
            {
                counter.fetch_add(1, Ordering::SeqCst);
            }
        }
        Ok(())
    });
    let listener = listener.unwrap();
    harness.resume().unwrap();
    let id = submit(&root, "go").await;
    wait(&harness, id).await;
    assert_eq!(result_text(&root).await, ("50".into(), false));
    // The journal's creation and a few groups, not one commit per result (51).
    assert!(commits.load(Ordering::SeqCst) < 10, "{} journal commits", commits.load(Ordering::SeqCst));
    drop(listener);
    harness.close(&ctx()).await.unwrap();
}
