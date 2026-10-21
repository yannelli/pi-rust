//! Scripts that resume after a restart, and what a rerun refuses to resume.

use std::sync::atomic::{AtomicU64, Ordering};

use pi_durable::codemode::{CODEMODE_JOURNAL_DOC, CODEMODE_RUN_DOC, JournalState, RunState};
use pi_durable::harness::tool::{ToolHookCall, ToolHooks, tool_task};
use pi_durable::session::Tx;

use super::*;

/// What `restart()` leaves: the reopened Harness and the code mode call's result.
pub struct Restarted {
    pub harness: Harness,
    pub result: (String, bool),
    _directory: tempfile::TempDir,
}

pub async fn no_change(_: Harness, _: TaskId) {}

pub async fn restart(chat: &Chat, code: &str) -> Restarted {
    restart_with(chat, code, no_change, no_change).await
}

/// Run `code` until `hold` is reached, close the Harness (a restart), let `between` change the stored state, then
/// reopen with `hold` no longer blocking and wait for the answer. `before_close` runs before the close.
pub async fn restart_with(
    chat: &Chat,
    code: &str,
    between: impl AsyncFnOnce(Harness, TaskId),
    before_close: impl AsyncFnOnce(Harness, TaskId),
) -> Restarted {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    chat.setup.faux.set_responses([script(code), done()]);
    let (harness, root) = open(&path, &chat.setup).await;
    let id = submit(&root, "go").await;
    chat.holds.reached().await;
    before_close(harness.clone(), script_task(&harness).await).await;
    harness.close(&ctx()).await.unwrap();
    chat.holds.block.store(false, Ordering::SeqCst);
    let (harness, root) = open(&path, &chat.setup).await;
    between(harness.clone(), script_task(&harness).await).await;
    harness.resume().unwrap();
    assert_eq!(wait(&harness, id).await, SubmissionStatus::Done);
    let result = result_text(&root).await;
    Restarted { harness, result, _directory: directory }
}

pub async fn edit_run(harness: &Harness, task: TaskId, edit: impl FnOnce(&mut RunState) + Send + 'static) {
    let change = move |tx: Tx| async move { tx.doc(&CODEMODE_RUN_DOC, task).await?.edit(edit) };
    harness.commit(change, &ctx()).await.unwrap();
}

pub async fn edit_journal(harness: &Harness, task: TaskId, edit: impl FnOnce(&mut JournalState) + Send + 'static) {
    let change = move |tx: Tx| async move { tx.doc(&CODEMODE_JOURNAL_DOC, task).await?.edit(edit) };
    harness.commit(change, &ctx()).await.unwrap();
}

/// Set the store the script starts with.
pub async fn set_store(harness: &Harness, task: TaskId, store: Value) {
    let Value::Object(store) = store else { unreachable!("a store is an object") };
    edit_run(harness, task, move |run| run.start.as_mut().unwrap().store = store).await;
}

pub async fn journal(harness: &Harness, task: TaskId) -> Option<JournalState> {
    let stored = harness.snapshot(&CODEMODE_JOURNAL_DOC, task, &ctx()).await.unwrap();
    stored.map(|journal| journal.deserialize_into().unwrap())
}

/// Wait until result 1's acknowledged count is `count`.
pub async fn acknowledged(harness: &Harness, task: TaskId, count: u64) {
    eventually(|| async {
        let journal = journal(harness, task).await;
        journal.and_then(|journal| journal.entries.first()?.issued_after) == Some(count)
    })
    .await;
}

#[tokio::test]
async fn resumes_a_script_after_a_restart_without_running_its_finished_calls_again() {
    let chat = codemode_chat(vec![]);
    let code = r#"const a = await tools.echo({ text: "a" });
        const held = await tools.hold({ id: 1 });
        const b = await tools.echo({ text: "b" });
        return [a, held, b, Date.now() > 0];"#;
    let restarted = restart(&chat, code).await;
    assert_eq!(restarted.result, (r#"["echo a","held 1","echo b",true]"#.into(), false));
    assert_eq!(echoes(&chat), BTreeMap::from([("a".into(), 1), ("b".into(), 1)]));
    // The VM ran twice; hold, replay-safe, reran once after the restart.
    assert_eq!(chat.vm.starts.load(Ordering::SeqCst), 2);
    assert_eq!(chat.holds.runs.load(Ordering::SeqCst), 2);
    restarted.harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn replays_results_in_the_order_they_arrived_not_the_order_the_calls_were_made() {
    let chat = codemode_chat(vec![]);
    let code = r#"const order = [];
        await Promise.all([tools.slow({ id: 1 }).then(() => order.push(1)), tools.slow({ id: 2 }).then(() => order.push(2))]);
        await tools.echo({ text: order.join(",") });
        await tools.hold({});
        return order;"#;
    // 2 finishes first, then 1.
    chat.slows.release(2);
    let slows = chat.slows.clone();
    tokio::spawn(async move {
        eventually(|| async { slows.finished.lock().unwrap().contains(&2) }).await;
        slows.release(1);
    });
    let restarted = restart(&chat, code).await;
    assert_eq!(restarted.result, ("[2,1]".into(), false));
    assert_eq!(echoes(&chat), BTreeMap::from([("2,1".into(), 1)]));
    restarted.harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn journals_globals_with_their_results_and_hands_a_replay_the_recorded_ones() {
    let counter = Arc::new(AtomicU64::new(0));
    let count = counter.clone();
    let next = CodemodeGlobal::new("next", move |_, _, _| {
        let value = count.fetch_add(1, Ordering::SeqCst) + 1;
        async move { Ok(Some(json!(value))) }
    });
    let chat = codemode_chat(vec![next]);
    let restarted = restart(&chat, "const n = await next(); await tools.hold({}); return [n, await next()];").await;
    // The first next() ran once and was replayed from the journal; the second ran after the restart.
    assert_eq!(restarted.result, ("[1,2]".into(), false));
    assert_eq!(counter.load(Ordering::SeqCst), 2);
    restarted.harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn keeps_the_script_s_clock_and_randomness_across_a_restart() {
    let chat = codemode_chat(vec![]);
    let code = r#"const before = [Date.now(), Math.random()];
        await tools.hold({});
        await tools.echo({ text: String(before[0]) + "/" + String(before[1]) });
        return "ok";"#;
    let restarted = restart(&chat, code).await;
    assert_eq!(restarted.result.0, "ok");
    // One echo: the same arguments, so the rerun admitted no second call.
    assert_eq!(echoes(&chat).into_values().collect::<Vec<_>>(), [1]);
    restarted.harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn keeps_a_global_s_undefined_result_undefined_live_and_replayed() {
    let chat = codemode_chat(vec![CodemodeGlobal::new("nothing", |_, _, _| async { Ok(None) })]);
    let code = "const before = await nothing(); await tools.hold({}); return [before === undefined, (await nothing()) === undefined];";
    let restarted = restart(&chat, code).await;
    assert_eq!(restarted.result, ("[true,true]".into(), false));
    restarted.harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn records_a_count_the_live_run_never_recorded_as_soon_as_a_replay_acknowledges_it() {
    let chat = codemode_chat(vec![]);
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    chat.setup.faux.set_responses([script(r#"await tools.echo({ text: "a" }); await tools.hold({});"#), done()]);
    let (harness, root) = open(&path, &chat.setup).await;
    let id = submit(&root, "go").await;
    chat.holds.reached().await;
    harness.close(&ctx()).await.unwrap();
    // As if result 1's count was never recorded; hold blocks again after the restart.
    *chat.holds.reached.lock().unwrap() = Deferred::default();
    let (harness, _) = open(&path, &chat.setup).await;
    let task = script_task(&harness).await;
    edit_journal(&harness, task, |journal| journal.entries[0].issued_after = None).await;
    harness.resume().unwrap();
    // The replay acknowledges result 1 and goes live; the script waits on hold, and the count is already recorded.
    chat.holds.reached().await;
    acknowledged(&harness, task, 2).await;
    harness.close(&ctx()).await.unwrap();
    chat.holds.block.store(false, Ordering::SeqCst);
    let (harness, _) = open(&path, &chat.setup).await;
    harness.resume().unwrap();
    assert_eq!(wait(&harness, id).await, SubmissionStatus::Done);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn refuses_to_resume_when_the_sandbox_s_fingerprint_or_the_code_mode_version_changed() {
    for field in ["fingerprint", "version"] {
        let chat = codemode_chat(vec![]);
        let change = async |harness: Harness, task: TaskId| {
            edit_run(&harness, task, move |run| {
                let start = run.start.as_mut().unwrap();
                match field {
                    "fingerprint" => start.fingerprint = Some("another sandbox".into()),
                    _ => start.version = 0,
                }
            })
            .await;
        };
        let code = r#"await tools.hold({}); await tools.echo({ text: "late" });"#;
        let restarted = restart_with(&chat, code, change, no_change).await;
        assert!(restarted.result.1);
        assert!(restarted.result.0.contains("cannot resume"), "{}", restarted.result.0);
        assert!(echoes(&chat).is_empty());
        restarted.harness.close(&ctx()).await.unwrap();
    }
}

#[tokio::test]
async fn times_a_script_out_across_a_restart_without_admitting_new_calls() {
    let chat = codemode_chat(vec![]);
    let code = "// @options: {\"timeout_ms\": 300}\nawait tools.hold({}); await tools.echo({ text: \"late\" });";
    let pause = async |_: Harness, _: TaskId| tokio::time::sleep(Duration::from_millis(400)).await;
    let restarted = restart_with(&chat, code, pause, no_change).await;
    assert!(restarted.result.1);
    assert!(restarted.result.0.contains("deadline passed"), "{}", restarted.result.0);
    assert!(echoes(&chat).is_empty());
    assert_eq!(chat.vm.starts.load(Ordering::SeqCst), 1);
    restarted.harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn returns_a_decided_outcome_after_a_restart_without_running_the_script_again() {
    let chat = codemode_chat(vec![]);
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    chat.holds.block.store(false, Ordering::SeqCst);
    chat.setup.faux.set_responses([script(r#"return "first""#), done()]);
    // Stop after execute() returned, so after the outcome is stored, and before the result commits.
    let (block, reached) = (Arc::new(AtomicBool::new(true)), Deferred::default());
    let (blocking, signal) = (block.clone(), reached.clone());
    let hooks = ToolHooks {
        before_tool: None,
        after_tool: Some(Arc::new(move |(call, _): (ToolHookCall, ToolExecutionResult), _, context: Context| {
            let stop = call.call.name == "codemode" && blocking.load(Ordering::SeqCst);
            let reached = signal.clone();
            Box::pin(async move {
                if stop {
                    reached.resolve();
                    aborted(context.abort_signal().unwrap()).await;
                }
                Ok(None)
            })
        })),
    };
    chat.setup.registry.install(Extension::new("hooks").hook(hook(&tool_task(), hooks))).unwrap();
    let (harness, root) = open(&path, &chat.setup).await;
    let id = submit(&root, "go").await;
    reached.wait().await;
    let task = script_task(&harness).await;
    let run = harness.snapshot(&CODEMODE_RUN_DOC, task, &ctx()).await.unwrap().unwrap();
    assert!(run.deserialize_into::<RunState>().unwrap().result.is_some());
    harness.close(&ctx()).await.unwrap();
    block.store(false, Ordering::SeqCst);
    let (harness, root) = open(&path, &chat.setup).await;
    harness.resume().unwrap();
    assert_eq!(wait(&harness, id).await, SubmissionStatus::Done);
    assert_eq!(result_text(&root).await, ("first".into(), false));
    assert_eq!(chat.vm.starts.load(Ordering::SeqCst), 1);
    harness.close(&ctx()).await.unwrap();
}
