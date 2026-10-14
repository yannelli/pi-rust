//! The remaining upstream "manual compaction" cases.

use pi_durable::harness::usage::{PROVIDER_DOC, USAGE_DOC};
use pi_durable::session::Tx;

use super::chat::*;

fn keep(tokens: u64) -> CompactionOverrides {
    CompactionOverrides { keep_recent_tokens: Some(tokens), ..manual() }
}

async fn status(submission: &Submission) -> (SubmissionStatus, Option<String>) {
    let record = submission.status(&ctx()).await.unwrap();
    (record.status, record.reason)
}

fn stale() -> (SubmissionStatus, Option<String>) {
    (SubmissionStatus::Unanswered, Some("stale".into()))
}

// Regression coverage for upstream #10424.
#[tokio::test]
async fn creates_provider_state_before_a_legacy_conversations_summarization_request() {
    let chat = open(OpenOptions::default()).await;
    chat.history().await;
    let id = chat.root.id;
    chat.root.commit(move |tx: Tx| async move { tx.retire_doc(&PROVIDER_DOC, id).await }, &ctx()).await.unwrap();
    assert!(chat.harness.snapshot(&PROVIDER_DOC, id, &ctx()).await.unwrap().is_none());
    chat.faux.summary(summary());
    let task = chat.compact(None).await;
    assert_eq!(chat.outcome(task).await["status"], "completed");
    let stored = chat.harness.snapshot(&PROVIDER_DOC, id, &ctx()).await.unwrap().unwrap().to_serde();
    let session_id = stored["sessionId"].as_str().unwrap().to_owned();
    // A UUID v7: version 7 and the RFC variant.
    let parts: Vec<&str> = session_id.split('-').collect();
    assert_eq!(parts.iter().map(|part| part.len()).collect::<Vec<_>>(), [8, 4, 4, 4, 12]);
    assert!(parts[2].starts_with('7') && "89ab".contains(&parts[3][..1]), "{session_id}");
    assert_eq!(chat.faux.summary_requests()[0].options.session_id.as_deref(), Some(session_id.as_str()));
    chat.close().await;
}

#[tokio::test]
async fn keeps_working_while_busy_and_places_the_summary_at_the_next_final_boundary() {
    let chat = open(OpenOptions::default()).await;
    chat.history().await;
    let (gate, reached) = (Deferred::default(), Deferred::default());
    chat.faux.agent(gated(&gate, answer("late answer"), Some(&reached)));
    let input = chat.submit("busy").await;
    reached.wait().await;
    chat.faux.summary(summary());
    let task = chat.compact(None).await;
    let placed = chat.placed(task).await;
    assert_eq!(status(&placed).await.0, SubmissionStatus::Queued);
    gate.resolve();
    assert_eq!(input.wait(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    assert_eq!(placed.wait(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    // The summary follows the answer; the kept range still includes the busy turn.
    assert_eq!(tail(&chat.kinds().await, 3), ["pi.user", "pi.assistant", "pi.compaction"]);
    chat.close().await;
}

#[tokio::test]
async fn places_a_queued_summary_at_post_tools_and_the_run_continues_in_the_compacted_context() {
    let chat = open(OpenOptions::default()).await;
    let wait = define_tool("wait", "wait", json!({ "type": "object" }), |_, _, _| async {
        Ok(ToolExecutionResult { output: Some(vec![pi_ai::ContentBlock::text("waited")]), ..Default::default() })
    });
    chat.setup.registry.install(Extension::new("tool:wait").tool(wait)).unwrap();
    let tools = AgentChange { tools: Change::Set(ToolFilter::List(vec!["wait".into()])), ..AgentChange::default() };
    chat.root.configure(tools, &ctx()).await.unwrap();
    chat.history().await;
    let (gate, reached) = (Deferred::default(), Deferred::default());
    let call = vec![faux_tool_call_with_id("wait", json!({}), "w1")];
    let calling = faux_assistant_message(call, FauxMessageOptions::stop_reason(StopReason::ToolUse));
    chat.faux.agent(gated(&gate, calling, Some(&reached)));
    chat.faux.agent(answer("after tools"));
    let input = chat.submit("use a tool").await;
    reached.wait().await;
    chat.faux.summary(summary());
    let task = chat.compact(None).await;
    chat.result(task).await;
    gate.resolve();
    assert_eq!(input.wait(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    // The continuation request starts with the summary.
    let continuation = chat.faux.last_agent_request();
    assert!(user_text(continuation.first()).contains("<summary>\nSUMMARY\n</summary>"));
    assert_eq!(tail(&chat.kinds().await, 4), ["pi.tool-result", "pi.compaction", "pi.system", "pi.assistant"]);
    chat.close().await;
}

#[tokio::test]
async fn runs_follow_ups_left_by_a_failed_run_after_placing_the_summary() {
    let chat = open(OpenOptions::default()).await;
    chat.history().await;
    let (gate, reached) = (Deferred::default(), Deferred::default());
    chat.faux.agent(gated(&gate, failure("bad request"), Some(&reached)));
    let failed = chat.submit("fails").await;
    reached.wait().await;
    let follow_up = chat.submit("follow-up").await;
    gate.resolve();
    assert_eq!(failed.wait(&ctx()).await.unwrap().status, SubmissionStatus::Unanswered);
    assert_eq!(status(&follow_up).await.0, SubmissionStatus::Queued);

    chat.faux.summary(summary());
    chat.faux.agent(answer("followed"));
    let task = chat.compact(None).await;
    chat.result(task).await;
    assert_eq!(follow_up.wait(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    let request = chat.faux.last_agent_request();
    assert!(first_user_text(&request).contains("SUMMARY"));
    assert!(request.iter().any(|message| user_text(Some(message)) == "follow-up"));
    chat.close().await;
}

#[tokio::test]
async fn settles_stale_when_a_reset_lands_while_it_summarizes() {
    let chat = open(OpenOptions::default()).await;
    chat.history().await;
    let (gate, reached) = (Deferred::default(), Deferred::default());
    chat.faux.summary(gated(&gate, summary(), Some(&reached)));
    let task = chat.compact(None).await;
    reached.wait().await;
    chat.root.reset(None, &ctx()).await.unwrap();
    gate.resolve();
    let placed = chat.placed(task).await;
    assert_eq!(status(&placed).await, stale());
    assert_eq!(chat.kinds().await.last().map(String::as_str), Some("pi.reset"));
    chat.close().await;
}

#[tokio::test]
async fn does_not_make_the_conversation_busy_a_submission_during_summarization_starts_its_run_at_once() {
    let chat = open(OpenOptions::default()).await;
    chat.history().await;
    let (summary_gate, summary_reached) = (Deferred::default(), Deferred::default());
    chat.faux.summary(gated(&summary_gate, summary(), Some(&summary_reached)));
    let task = chat.compact(None).await;
    summary_reached.wait().await;
    let (answer_gate, answer_reached) = (Deferred::default(), Deferred::default());
    chat.faux.agent(gated(&answer_gate, answer("a4"), Some(&answer_reached)));
    let input = chat.submit("u4").await;
    // Placed and answered immediately with the uncompacted context, not queued behind the compaction.
    assert_eq!(status(&input).await.0, SubmissionStatus::Placed);
    answer_reached.wait().await;
    assert_eq!(first_user_text(&chat.faux.last_agent_request()), text("u1", 100));
    // The summary is ready while the run is busy, so it queues and lands after the answer.
    summary_gate.resolve();
    let placed = chat.placed(task).await;
    assert_eq!(status(&placed).await.0, SubmissionStatus::Queued);
    answer_gate.resolve();
    assert_eq!(input.wait(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    assert_eq!(placed.wait(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    assert_eq!(tail(&chat.kinds().await, 3), ["pi.user", "pi.assistant", "pi.compaction"]);
    chat.close().await;
}

async fn ledger_input(chat: &Chat) -> u64 {
    let usage = chat.harness.snapshot(&USAGE_DOC, chat.root.id, &ctx()).await.unwrap().unwrap().to_serde();
    usage["models"]["faux/faux-1"]["input"].as_u64().unwrap()
}

#[tokio::test]
async fn counts_the_spend_of_a_summary_that_ends_stale_and_writes_no_entry_for_it() {
    let chat = open(OpenOptions::default()).await;
    chat.history().await;
    let before = ledger_input(&chat).await;
    let (gate, reached) = (Deferred::default(), Deferred::default());
    chat.faux.summary(gated(&gate, summary(), Some(&reached)));
    let task = chat.compact(None).await;
    reached.wait().await;
    chat.root.reset(None, &ctx()).await.unwrap();
    gate.resolve();
    let placed = chat.placed(task).await;
    assert_eq!(status(&placed).await, stale());
    assert!(ledger_input(&chat).await > before);
    assert!(!chat.kinds().await.contains(&"pi.compaction".to_owned()));
    chat.close().await;
}

async fn first_text(chat: &Chat) -> String {
    first_user_text(&chat.root.context(&ctx(), None).await.unwrap().messages)
}

#[tokio::test]
async fn lets_the_compaction_that_cuts_furthest_win_whatever_finishes_first() {
    let chat = open(OpenOptions::default()).await;
    chat.history().await;
    let (first, first_reached) = (Deferred::default(), Deferred::default());
    chat.faux.summary(gated(&first, answer("FIRST"), Some(&first_reached)));
    let early = chat.compact(None).await;
    first_reached.wait().await;
    chat.turn(&text("u4", 100), &text("a4", 100)).await;
    chat.faux.summary(answer("SECOND"));
    let later = chat.compact(None).await;
    assert_eq!(chat.outcome(later).await["status"], "completed");
    first.resolve();
    // The early compaction cut at u3, before the later cut at u4.
    let placed = chat.placed(early).await;
    assert_eq!(status(&placed).await, stale());
    assert!(first_text(&chat).await.contains("SECOND"));
    chat.close().await;
}

#[tokio::test]
async fn places_an_older_selected_summary_that_cuts_later_than_the_newer_one() {
    let chat = open(OpenOptions::default()).await;
    chat.history().await;
    // A: small budget, late cut; selected first, finishes last.
    let (gate, reached) = (Deferred::default(), Deferred::default());
    chat.faux.summary(gated(&gate, answer("A"), Some(&reached)));
    let a = chat.compact(None).await;
    reached.wait().await;
    // B: larger budget, earlier cut; placed first.
    chat.setup.settings.lock().unwrap().compaction = Some(keep(350));
    chat.faux.summary(answer("B"));
    let b = chat.compact(None).await;
    chat.result(b).await;
    assert!(first_text(&chat).await.contains('B'));
    gate.resolve();
    let placed = chat.placed(a).await;
    assert_eq!(status(&placed).await.0, SubmissionStatus::Done);
    let messages = chat.root.context(&ctx(), None).await.unwrap().messages;
    assert!(first_user_text(&messages).contains("<summary>\nA\n</summary>"));
    let rest: Vec<String> = messages[1..].iter().map(|message| user_text(Some(message))).collect();
    assert_eq!(rest, [text("u3", 100), text("a3", 100)]);
    chat.close().await;
}

/// Two summaries queued in one boundary, selected with the given kept budgets.
async fn two_queued(keeps: [u64; 2]) -> Vec<SubmissionStatus> {
    let chat = open(OpenOptions::default()).await;
    chat.history().await;
    let (gate, reached) = (Deferred::default(), Deferred::default());
    chat.faux.agent(gated(&gate, answer("done"), Some(&reached)));
    let input = chat.submit("busy").await;
    reached.wait().await;
    let mut submissions = Vec::new();
    for (index, tokens) in keeps.into_iter().enumerate() {
        chat.setup.settings.lock().unwrap().compaction = Some(keep(tokens));
        chat.faux.summary(answer(&format!("S{index}")));
        let task = chat.compact(None).await;
        submissions.push(chat.placed(task).await);
    }
    gate.resolve();
    input.wait(&ctx()).await.unwrap();
    let mut statuses = Vec::new();
    for submission in submissions {
        statuses.push(submission.wait(&ctx()).await.unwrap().status);
    }
    chat.close().await;
    statuses
}

#[tokio::test]
async fn places_two_queued_summaries_in_one_boundary_when_the_second_cuts_before_the_first() {
    assert_eq!(two_queued([150, 350]).await, [SubmissionStatus::Done, SubmissionStatus::Unanswered]);
}

#[tokio::test]
async fn places_two_queued_summaries_in_one_boundary_when_the_second_cuts_at_the_first() {
    assert_eq!(two_queued([150, 150]).await, [SubmissionStatus::Done, SubmissionStatus::Done]);
}

#[tokio::test]
async fn places_two_queued_summaries_in_one_boundary_when_the_second_cuts_after_the_first() {
    assert_eq!(two_queued([350, 150]).await, [SubmissionStatus::Done, SubmissionStatus::Done]);
}

#[tokio::test]
async fn is_aborted_by_conversation_abort_an_already_queued_summary_survives_it() {
    let chat = open(OpenOptions::default()).await;
    chat.history().await;
    let reached = Deferred::default();
    chat.faux.summary(gated(&Deferred::default(), summary(), Some(&reached)));
    let task = chat.compact(None).await;
    reached.wait().await;
    assert_eq!(chat.live().await["compactions"].as_array().map(Vec::len), Some(1));
    chat.root.abort(&ctx(), false).await.unwrap();
    assert_eq!(chat.outcome(task).await["status"], "aborted");
    assert!(chat.live().await.get("compactions").is_none());
    assert!(!chat.kinds().await.contains(&"pi.compaction".to_owned()));

    // Queued while busy, then Esc: the queued write stays and lands with the next run's boundary.
    let (gate, busy) = (Deferred::default(), Deferred::default());
    chat.faux.agent(gated(&gate, answer("never"), Some(&busy)));
    chat.submit("busy").await;
    busy.wait().await;
    chat.faux.summary(summary());
    let queued = chat.compact(None).await;
    let submission = chat.placed(queued).await;
    chat.root.abort(&ctx(), false).await.unwrap();
    assert_eq!(status(&submission).await.0, SubmissionStatus::Queued);
    assert_eq!(submission.abort(&ctx()).await.unwrap(), AbortSubmissionResult::Aborted);
    chat.turn("next", "ok").await;
    assert_eq!(status(&submission).await, (SubmissionStatus::Unanswered, Some("aborted".into())));
    assert!(!chat.kinds().await.contains(&"pi.compaction".to_owned()));
    chat.close().await;
}

#[tokio::test]
async fn is_ordinary_work_idle_waits_include_it() {
    let chat = open(OpenOptions::default()).await;
    chat.history().await;
    let (gate, reached) = (Deferred::default(), Deferred::default());
    chat.faux.summary(gated(&gate, summary(), Some(&reached)));
    let task = chat.compact(None).await;
    reached.wait().await;
    let root = chat.root.clone();
    let idle = tokio::spawn(async move { root.wait_for_idle(&ctx()).await });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert!(!idle.is_finished());
    gate.resolve();
    idle.await.unwrap().unwrap();
    let record = chat.harness.get_task(task, &ctx()).await.unwrap().unwrap();
    assert_eq!(record.state.status(), pi_durable::TaskStatus::Terminal);
    chat.close().await;
}

#[tokio::test]
async fn enables_scheduling_right_after_open() {
    let setup = chat_setup(Default::default());
    let faux = script(&setup);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    setup.settings.lock().unwrap().compaction = Some(keep(10));
    let id = root.compact(None, &ctx()).await.unwrap();
    // get_task() only reads, so progress here comes from compact() itself.
    wait_for(|| {
        let harness = harness.clone();
        async move {
            let record = harness.get_task(id, &ctx()).await.unwrap().unwrap();
            record.state.status() == pi_durable::TaskStatus::Terminal
        }
    })
    .await;
    let record = harness.get_task(id, &ctx()).await.unwrap().unwrap();
    assert_eq!(serde_json::to_value(record.state.outcome()).unwrap(), json!({ "status": "completed", "result": {} }));
    assert!(faux.summary_requests().is_empty());
    harness.close(&ctx()).await.unwrap();
}
