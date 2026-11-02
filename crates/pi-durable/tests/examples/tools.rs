//! Examples 17 to 20: coding tools with a timing hook, a print mode, JSON op and state streams, and the inbox.

use std::sync::Mutex;
use std::time::Instant;

use pi_ai::StopReason;
use pi_durable::entries::{ASSISTANT_ENTRY, TOOL_RESULT_ENTRY};
use pi_durable::env::{LocalEnvOptions, LocalExecutionEnv, SharedEnv};
use pi_durable::harness::inbox::INBOX_DOC;
use pi_durable::harness::tool::{ToolHooks, tool_task};
use pi_durable::storage::sqlite::open_sqlite_storage;
use pi_durable::storage::{JsonlStorageOptions, open_local_jsonl_storage};
use pi_durable::testing::faux::{
    FauxMessageOptions, FauxResponseStep, RegisterFauxProviderOptions, faux_assistant_message, faux_text,
    faux_tool_call_with_id,
};
use pi_durable::tools::{BashToolOptions, ReadToolOptions, coding_tools, create_bash_tool, create_read_tool};
use support::chat::answer;

use super::*;
use crate::support::{ValueCall, on_value};

fn tool_call(name: &str, args: Value, id: &str) -> FauxResponseStep {
    let call = faux_tool_call_with_id(name, args, id);
    faux_assistant_message(vec![call], FauxMessageOptions::stop_reason(StopReason::ToolUse)).into()
}

fn local_env(cwd: &str) -> SharedEnv {
    Arc::new(LocalExecutionEnv::new(LocalEnvOptions::new(cwd)))
}

async fn answer_text(root: &Conversation, submission: Submission) -> String {
    let settled = submission.wait(&ctx()).await.unwrap();
    assert_eq!(settled.status, SubmissionStatus::Done);
    let answer = settled.answer.unwrap();
    let entry = root.commit(move |tx: Tx| async move { tx.entry(answer).await }, &ctx()).await.unwrap();
    assert!(ASSISTANT_ENTRY.is(entry.as_ref()));
    text_of(entry.and_then(|entry| entry.model).unwrap_or_default().first()).unwrap_or_default()
}

fn is_big_cat(call: &pi_ai::ToolCall) -> bool {
    call.name == "bash" && call.arguments.get("command").and_then(Value::as_str).is_some_and(|c| c.contains("1gb.txt"))
}

#[tokio::test]
async fn ex17_coding_tools_edit_a_file_and_time_a_command() {
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path().to_string_lossy().into_owned();
    std::fs::write(directory.path().join("notes.txt"), "hello world\n").unwrap();
    let setup = chat_setup(Default::default());
    let edit = json!({ "path": "notes.txt", "edits": [{ "oldText": "world", "newText": "durable" }] });
    setup.faux.set_responses([
        tool_call("read", json!({ "path": "notes.txt" }), "r"),
        tool_call("edit", edit, "e"),
        tool_call("bash", json!({ "command": "cat notes.txt" }), "b"),
        tool_call("bash", json!({ "command": "cat /tmp/1gb.txt" }), "c"),
        answer("The file now greets durable."),
    ]);
    let (start, took) = (Arc::new(Mutex::new(None::<Instant>)), Arc::new(Mutex::new(None)));
    let (begin, end) = (start.clone(), took.clone());
    let timing = ToolHooks {
        before_tool: Some(Arc::new(move |call, _, _| {
            if is_big_cat(&call.call) {
                *begin.lock().unwrap() = Some(Instant::now());
            }
            Box::pin(async { Ok(None) })
        })),
        after_tool: Some(Arc::new(move |(call, _), _, _| {
            if is_big_cat(&call.call) {
                *end.lock().unwrap() = start.lock().unwrap().map(|start| start.elapsed());
            }
            Box::pin(async { Ok(None) })
        })),
    };
    setup.registry.install(coding_tools()).unwrap();
    setup.registry.install(Extension::new("timing").hook(hook(&tool_task(), timing))).unwrap();
    let storage = open_local_jsonl_storage(&cwd, &ctx(), JsonlStorageOptions::default()).await.unwrap();
    let options = setup.options().with_env(|target, _| {
        let env = local_env(&target.cwd.unwrap_or_else(|| ".".into()));
        Box::pin(async move { Ok(Some(env)) })
    });
    let (harness, root) = open_chat_with(Arc::new(storage), &setup, options).await;
    root.configure(AgentChange { cwd: Change::Set(cwd.clone()), ..Default::default() }, &ctx()).await.unwrap();
    let submission = root.submit(SubmissionDraft::input("Greet durable instead."), &ctx()).await.unwrap();
    assert_eq!(answer_text(&root, submission).await, "The file now greets durable.");
    let results: Vec<(String, String)> = all_entries(&root)
        .await
        .into_iter()
        .filter(|entry| TOOL_RESULT_ENTRY.is(Some(entry)))
        .map(|entry| match entry.model.unwrap_or_default().first() {
            Some(pi_ai::Message::ToolResult(result)) => (result.tool_name.clone(), block_text(&result.content)),
            _ => panic!("tool result entries hold one tool result"),
        })
        .collect();
    let names: Vec<&str> = results.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["read", "edit", "bash", "bash"]);
    assert!(results[0].1.contains("hello world") && results[2].1.contains("hello durable"), "{results:?}");
    assert!(took.lock().unwrap().is_some(), "the timing hook saw the big cat");
    let file = std::fs::read_to_string(directory.path().join("notes.txt")).unwrap();
    assert_eq!(file, "hello durable\n");
    harness.close(&ctx()).await.unwrap();
}

/// The joined text blocks of a tool result.
fn block_text(content: &[pi_ai::ContentBlock]) -> String {
    content.iter().filter_map(pi_ai::ContentBlock::as_text).collect()
}

fn coding_extension() -> Extension {
    let preamble = section(
        "preamble",
        |_, _| std::future::ready(Ok(Some("You are a concise coding assistant.".into()))),
        Some(false),
    );
    let bash = create_bash_tool(BashToolOptions { command_prefix: None, prepare: None });
    Extension::new("coding").tool(create_read_tool(ReadToolOptions::default())).tool(bash).section(preamble)
}

const LISTING: &str = "This directory holds the durable package sources, tests, and docs.";

fn listing_responses(setup: &ChatSetup) {
    setup.faux.set_responses([tool_call("bash", json!({ "command": "ls" }), "call-1"), answer(LISTING)]);
}

// Upstream picks a real provider when `OPENAI_API_KEY` is set; these tests always use the faux one.
#[tokio::test]
async fn ex18_print_prints_the_final_answer() {
    let directory = tempfile::tempdir().unwrap();
    let env = local_env(&directory.path().to_string_lossy());
    let setup = chat_setup(Default::default());
    listing_responses(&setup);
    setup.registry.install(coding_extension()).unwrap();
    let (harness, root) = open_chat_with(memory(), &setup, setup.options().with_shared_env(env)).await;
    let submission = root.submit(SubmissionDraft::input("What is in this directory?"), &ctx()).await.unwrap();
    assert_eq!(answer_text(&root, submission).await, LISTING);
    harness.close(&ctx()).await.unwrap();
}

/// Run the listing prompt and collect each printed JSON line: the view and its ops, or entries since and `pi.live`.
async fn run_json(storage: Arc<dyn Storage>, ops: bool) -> Vec<Value> {
    let directory = tempfile::tempdir().unwrap();
    let env = local_env(&directory.path().to_string_lossy());
    let setup = chat_setup(RegisterFauxProviderOptions { tokens_per_second: Some(200.0), ..Default::default() });
    listing_responses(&setup);
    setup.registry.install(coding_extension()).unwrap();
    let (harness, root) = open_chat_with(storage, &setup, setup.options().with_shared_env(env)).await;
    let printed = Arc::new(Mutex::new(Vec::new()));
    let sink = printed.clone();
    let watch = root.watch(&ctx(), None).await.unwrap();
    let view = watch.value().to_serde();
    if ops {
        printed.lock().unwrap().push(json!({ "view": view }));
        let listen = on_value(move |ValueCall { ops, .. }, _| {
            sink.lock().unwrap().push(json!({ "ops": serde_json::to_value(ops).unwrap() }));
            async { Ok(()) }
        });
        watch.start(listen).unwrap();
    } else {
        let newest = Mutex::new(newest_id(&view));
        printed.lock().unwrap().push(json!({ "entries": view["entries"], "live": view["docs"]["pi.live"] }));
        let listen = on_value(move |ValueCall { value, .. }, _| {
            let value = value.to_serde();
            let mut newest = newest.lock().unwrap();
            let since = *newest;
            let entries: Vec<&Value> =
                value["entries"].as_array().unwrap().iter().filter(|entry| entry["id"].as_u64() > since).collect();
            *newest = newest_id(&value).max(since);
            let mut line = json!({ "live": value["docs"]["pi.live"] });
            if !entries.is_empty() {
                line["entries"] = json!(entries);
            }
            sink.lock().unwrap().push(line);
            async { Ok(()) }
        });
        watch.start(listen).unwrap();
    }
    let submission = root.submit(SubmissionDraft::input("What is in this directory?"), &ctx()).await.unwrap();
    submission.wait(&ctx()).await.unwrap();
    harness.wait_for_idle(&ctx()).await.unwrap();
    tokio::task::yield_now().await;
    watch.stop().await;
    harness.close(&ctx()).await.unwrap();
    std::mem::take(&mut *printed.lock().unwrap())
}

fn newest_id(view: &Value) -> Option<u64> {
    view["entries"].as_array().and_then(|entries| entries.last()).and_then(|entry| entry["id"].as_u64())
}

fn entry_kinds(lines: &[Value]) -> Vec<String> {
    let entries = lines.iter().flat_map(|line| line["entries"].as_array().cloned().unwrap_or_default());
    entries.filter_map(|entry| Some(entry["kind"].as_str()?.to_owned())).collect()
}

#[tokio::test]
async fn ex19_json_prints_state_over_sqlite_and_jsonl_and_ops_over_memory() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("json.sqlite").to_string_lossy().into_owned();
    let sqlite = run_json(Arc::new(open_sqlite_storage(&path).await.unwrap()), false).await;
    let jsonl_path = directory.path().join("jsonl").to_string_lossy().into_owned();
    std::fs::create_dir_all(&jsonl_path).unwrap();
    let jsonl = open_local_jsonl_storage(&jsonl_path, &ctx(), JsonlStorageOptions::default()).await.unwrap();
    let jsonl = run_json(Arc::new(jsonl), false).await;
    for lines in [&sqlite, &jsonl] {
        let kinds = entry_kinds(lines);
        for expected in ["pi.user", "pi.assistant", "pi.tool-result"] {
            assert!(kinds.iter().any(|kind| kind == expected), "{expected} in {kinds:?}");
        }
        assert!(lines.iter().any(|line| line["live"]["tools"].is_array()), "a running tool in {lines:?}");
        assert!(lines.last().unwrap()["live"].get("run").is_none(), "{:?}", lines.last());
    }
    let ops = run_json(memory(), true).await;
    assert!(ops[0]["view"].is_object());
    assert!(ops.len() > 2 && ops[1..].iter().all(|line| line["ops"].is_array()));
}

#[tokio::test]
async fn ex20_inbox_steers_follows_up_writes_withdraws_and_rejects_while_busy() {
    let setup = chat_setup(Default::default());
    let held = Arc::new(tokio::sync::Notify::new());
    let gate = held.clone();
    let slow = FauxResponseStep::async_factory(move |_, _, _, _| {
        let gate = gate.clone();
        Box::pin(async move {
            gate.notified().await;
            Ok(faux_assistant_message(vec![faux_text("Answer to the first question.")], FauxMessageOptions::default()))
        })
    });
    setup.faux.set_responses([slow, answer("Answer to the follow-up and the steer.")]);
    setup.settings.lock().unwrap().follow_up_mode = Some(QueueMode::All);
    let (harness, root) = open_chat(memory(), &setup).await;
    let context = ctx();
    let submit = |draft| root.submit(draft, &context);
    let first = submit(SubmissionDraft::input("First question")).await.unwrap();
    let follow_up = submit(SubmissionDraft::input("A follow-up")).await.unwrap();
    let steer = submit(SubmissionDraft::input("A steer").with_when_busy(WhenBusy::Steer)).await.unwrap();
    let note = submit(SubmissionDraft::write(queued_note("noted while busy"))).await.unwrap();
    let withdrawn = submit(SubmissionDraft::input("Never mind")).await.unwrap();
    let rejected = submit(SubmissionDraft::input("Now or never").with_when_busy(WhenBusy::Reject)).await;
    assert!(rejected.err().unwrap().to_string().contains("busy"));
    assert_eq!(withdrawn.abort(&ctx()).await.unwrap(), AbortSubmissionResult::Aborted);
    let inbox = harness.snapshot(&INBOX_DOC, root.id, &ctx()).await.unwrap().unwrap().to_serde();
    let modes: Vec<Value> = inbox["items"].as_array().unwrap().iter().map(|item| item["mode"].clone()).collect();
    assert_eq!(modes, [json!("followUp"), json!("steer"), json!("write")]);
    held.notify_one();
    let mut statuses = Vec::new();
    for submission in [first, follow_up, steer, note, withdrawn] {
        let record = submission.wait(&ctx()).await.unwrap();
        statuses.push((record.status, record.reason));
    }
    let done = (SubmissionStatus::Done, None);
    let unanswered = (SubmissionStatus::Unanswered, Some("aborted".to_owned()));
    assert_eq!(statuses, [done.clone(), done.clone(), done.clone(), done, unanswered]);
    let kinds = kinds(&all_entries(&root).await);
    // The note lands at the turn boundary, and the steer goes ahead of the follow-up in one batch.
    assert_eq!(kinds, ["pi.user", "pi.assistant", "note", "pi.user", "pi.user", "pi.assistant"]);
    harness.close(&ctx()).await.unwrap();
}
