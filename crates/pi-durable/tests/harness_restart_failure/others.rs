//! Upstream "other tasks cut off restartLimit times in a row": a tool call and a compaction end visibly.

use std::sync::atomic::{AtomicUsize, Ordering};

use pi_ai::ContentBlock;

use super::support::Deferred;
use super::support::chat::{add_tool, call, no_parameters};
use super::support::task_support::aborted;
use super::*;

fn infos(setup: &ChatSetup) -> Vec<Value> {
    setup.infos.lock().unwrap().iter().map(|info| serde_json::to_value(info).unwrap()).collect()
}

#[tokio::test]
async fn ends_a_tool_call_with_an_error_result_instead_of_running_it_again() {
    let storage = Arc::new(ControlledStorage::default());
    let setup = chat_setup(Default::default());
    restart_limit(&setup, 2.0);
    let progress =
        ProgressOverrides { partial_interval_ms: None, output_interval_ms: Some(0), persist_within_ms: Some(0.0) };
    setup.settings.lock().unwrap().progress = Some(progress);
    let runs = Arc::new(AtomicUsize::new(0));
    let running = Arc::new(Mutex::new(Deferred::default()));
    let (counter, current) = (runs.clone(), running.clone());
    let heavy = define_tool("heavy", "Never finishes", no_parameters(), move |_, api, context| {
        counter.fetch_add(1, Ordering::SeqCst);
        let started = current.lock().unwrap().clone();
        async move {
            api.output("partial\n")?;
            started.resolve();
            aborted(context.abort_signal().unwrap()).await;
            Ok(ToolExecutionResult::default())
        }
    })
    .with_replay(ToolReplay::Safe);
    add_tool(&setup, heavy);
    let saw = FauxResponseStep::factory(|request, _, _, _| {
        let result = text_of(request.messages.last()).unwrap_or_default();
        faux_assistant_message(format!("saw: {result}").as_str(), FauxMessageOptions::default())
    });
    setup.faux.set_responses([call("heavy", "c1"), saw]);
    let (mut harness, mut root) = open(&storage, &setup).await;
    harness.resume().unwrap();
    let id = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().id;
    for _ in 1..=2 {
        let started = running.lock().unwrap().clone();
        started.wait().await;
        *running.lock().unwrap() = Deferred::default();
        stored().await;
        kill(&harness).await;
        (harness, root) = open(&storage, &setup).await;
        harness.resume().unwrap();
    }
    assert_eq!(settled(&harness, id).await.status, SubmissionStatus::Done);
    assert_eq!(runs.load(Ordering::SeqCst), 2);
    let result = all_entries(&root).await.into_iter().find(|entry| entry.kind == "pi.tool-result").unwrap();
    let Some(Message::ToolResult(message)) = result.model.as_ref().and_then(|model| model.first()) else {
        panic!("a tool result message");
    };
    assert!(message.is_error);
    let diagnostics = result.data.as_ref().map(Json::to_serde).unwrap()["diagnostics"].clone();
    let message_text = "Tool heavy was interrupted 2 times in a row; it is not run again";
    assert_eq!(diagnostics, json!([{ "severity": "error", "code": "restart_limit", "message": message_text }]));
    // The slot's partial output is kept, as for an interrupted call.
    let text: String = message.content.iter().filter_map(ContentBlock::as_text).collect();
    assert!(text.contains("partial"), "{text}");
    let query = TaskQuery { kind: Some("pi.tool".into()), ..TaskQuery::default() };
    let scan = move |tx: pi_durable::session::Tx| async move { tx.scan_tasks(query, 10, None).await };
    let tool = harness.commit(scan, &ctx()).await.unwrap().items.remove(0);
    let state = serde_json::to_value(&tool.state).unwrap();
    assert_eq!((&state["status"], &state["outcome"]["status"]), (&json!("terminal"), &json!("failed")));
    assert_eq!(state["outcome"]["error"]["detail"], json!({ "reason": "restart_limit" }));
    let task = json!({ "id": tool.id, "kind": "pi.tool", "conversationId": ROOT_CONVERSATION_ID });
    let interrupted = json!({ "kind": "task_interrupted", "task": task, "reason": "restart_limit", "restarts": 2 });
    assert!(infos(&setup).contains(&interrupted), "{:?}", infos(&setup));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn fails_a_compaction_into_compaction_failure() {
    let storage = Arc::new(ControlledStorage::default());
    let setup = chat_setup(Default::default());
    restart_limit(&setup, 1.0);
    // Little recent context is kept, so the first turn is summarized.
    let compaction = CompactionOverrides { enabled: Some(false), keep_recent_tokens: Some(10), ..Default::default() };
    setup.settings.lock().unwrap().compaction = Some(compaction);
    setup.faux.set_responses([answer("a1"), answer("a2")]);
    let (harness, root) = open(&storage, &setup).await;
    harness.resume().unwrap();
    for text in ["u1 ", "u2 "] {
        let submission = root.submit(SubmissionDraft::input(text.repeat(200)), &ctx()).await.unwrap();
        submission.wait(&ctx()).await.unwrap();
    }
    // The summary request never returns.
    let (hanging, reached) = unanswered();
    setup.faux.set_responses([hanging]);
    let compaction = root.compact(None, &ctx()).await.unwrap();
    reached.notified().await;
    kill(&harness).await;

    let (harness, _) = open(&storage, &setup).await;
    harness.resume().unwrap();
    let error = "Compaction was interrupted once";
    let outcome = harness.wait_for_task(compaction, &ctx()).await.unwrap().state;
    let outcome = serde_json::to_value(outcome.outcome()).unwrap();
    assert_eq!(
        outcome,
        json!({ "status": "failed", "error": { "message": error, "detail": { "reason": "restart_limit" } } })
    );
    let state = live(&harness).await;
    assert_eq!(state["compactionFailure"], json!({ "taskId": compaction, "trigger": "manual", "error": error }));
    assert!(state.get("compactions").is_none(), "{state}");
    wait_for(|| async { !setup.infos.lock().unwrap().is_empty() }).await;
    let task = json!({ "id": compaction, "kind": "pi.compaction", "conversationId": ROOT_CONVERSATION_ID });
    let interrupted = json!({ "kind": "task_interrupted", "task": task, "reason": "restart_limit", "restarts": 1 });
    assert!(infos(&setup).contains(&interrupted), "{:?}", infos(&setup));
    harness.close(&ctx()).await.unwrap();
}
