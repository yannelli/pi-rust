//! The upstream "background threshold compaction" and "blocking threshold compaction" cases.

use pi_durable::TaskOutcome;
use pi_durable::harness::compaction::CompactionDecision;
use pi_durable::harness::live::settle_scheduler_outcome;
use pi_durable::session::Tx;

use super::chat::*;
use super::outcomes::before_compact;

#[tokio::test]
async fn starts_above_the_background_threshold_without_blocking_the_run_idle_waits_and_esc_ignore_it() {
    let chat = open(window(2000)).await;
    chat.history().await;
    chat.set_policy(background());
    let (gate, reached) = (Deferred::default(), Deferred::default());
    chat.faux.summary(gated(&gate, summary(), Some(&reached)));
    chat.turn(&text("u4", 100), &text("a4", 100)).await;
    reached.wait().await;
    let task = chat.compaction_tasks().await.remove(0);
    assert!(task.background);
    assert_eq!(task.input.get("reason").and_then(Json::as_str), Some("threshold"));
    assert!(task.owner.is_none());
    let expected = json!([{ "taskId": task.id, "trigger": "threshold", "blocking": false, "attempt": 1 }]);
    assert_eq!(chat.live().await["compactions"], expected);
    // Background work: neither conversation idle nor Esc waits for or stops it.
    chat.root.wait_for_idle(&ctx()).await.unwrap();
    chat.root.abort(&ctx(), false).await.unwrap();
    let record = chat.harness.get_task(task.id, &ctx()).await.unwrap().unwrap();
    assert_eq!(record.state.status(), pi_durable::TaskStatus::Running);
    gate.resolve();
    assert_eq!(chat.outcome(task.id).await["status"], "completed");
    assert_eq!(chat.kinds().await.last().map(String::as_str), Some("pi.compaction"));
    assert!(chat.first_text().await.contains("SUMMARY"));
    chat.close().await;
}

async fn does_not_start(policy: CompactionOverrides) {
    let chat = open(window(2000)).await;
    chat.history().await;
    chat.set_policy(policy);
    chat.turn(&text("u4", 100), &text("a4", 100)).await;
    assert!(chat.compaction_tasks().await.is_empty());
    assert!(chat.faux.summary_requests().is_empty());
    chat.close().await;
}

#[tokio::test]
async fn does_not_start_when_disabled() {
    does_not_start(CompactionOverrides { enabled: Some(false), ..background() }).await;
}

#[tokio::test]
async fn does_not_start_when_background_tokens_is_0() {
    does_not_start(CompactionOverrides { background_tokens: Some(0), ..background() }).await;
}

#[tokio::test]
async fn does_not_start_when_there_is_no_cut() {
    does_not_start(CompactionOverrides { keep_recent_tokens: Some(100_000), ..background() }).await;
}

#[tokio::test]
async fn does_not_start_while_another_compaction_is_listed() {
    let chat = open(window(2000)).await;
    chat.history().await;
    let reached = Deferred::default();
    chat.faux.summary(gated(&Deferred::default(), summary(), Some(&reached)));
    let manual = chat.compact(None).await;
    reached.wait().await;
    chat.set_policy(background());
    chat.turn(&text("u4", 100), &text("a4", 100)).await;
    let ids: Vec<TaskId> = chat.compaction_tasks().await.iter().map(|task| task.id).collect();
    assert_eq!(ids, [manual]);
    chat.harness.abort_task(manual, &ctx()).await.unwrap();
    chat.close().await;
}

#[tokio::test]
async fn records_a_failure_in_live_in_the_commit_that_ends_the_compaction_until_a_summary_is_produced() {
    let chat = open(window(2000)).await;
    chat.history().await;
    chat.set_policy(background());
    let views = views(&chat).await;
    chat.faux.summary(failure("bad request"));
    chat.turn(&text("u4", 100), &text("a4", 100)).await;
    wait_for(|| async { chat.live().await.get("compactionFailure").is_some() }).await;
    let recorded = chat.live().await["compactionFailure"].clone();
    assert_eq!(recorded["trigger"], "threshold");
    assert_eq!(recorded["error"], "Summarization failed: bad request");
    // The value that drops the compaction's status records its failure.
    let ended = |task: &Value| {
        let states = views.live();
        let lists =
            |state: &Value| state["compactions"].as_array().is_some_and(|all| all.iter().any(|s| s["taskId"] == *task));
        let listed = states.iter().position(lists).unwrap();
        states.into_iter().enumerate().find(|(index, state)| *index > listed && !lists(state)).unwrap().1
    };
    assert_eq!(ended(&recorded["taskId"])["compactionFailure"], recorded);
    // A late watch starts with it.
    assert_eq!(watched_live(&chat).await["compactionFailure"], recorded);
    // A compaction that produces a summary removes the failure in the commit that ends it.
    chat.set_policy(manual());
    chat.faux.summary(summary());
    let manual_task = chat.compact(None).await;
    assert_eq!(chat.outcome(manual_task).await["status"], "completed");
    assert!(chat.live().await.get("compactionFailure").is_none());
    assert!(ended(&json!(manual_task)).get("compactionFailure").is_none());
    views.detach();
    chat.close().await;
}

#[tokio::test]
async fn records_the_error_of_a_compaction_the_scheduler_faults_or_orphans() {
    let orphaned = TaskOutcome::Orphaned { reason: "definition removed".into() };
    for (outcome, error) in
        [(TaskOutcome::faulted("no credentials"), "no credentials"), (orphaned, "definition removed")]
    {
        let chat = open(window(2000)).await;
        chat.history().await;
        chat.set_policy(background());
        let reached = Deferred::default();
        chat.faux.summary(gated(&Deferred::default(), summary(), Some(&reached)));
        chat.turn(&text("u4", 100), &text("a4", 100)).await;
        reached.wait().await;
        let record = chat.compaction_tasks().await.remove(0);
        let settle = |tx: Tx| async move { settle_scheduler_outcome(&tx, &record, &outcome).await };
        chat.root.commit(settle, &ctx()).await.unwrap();
        let live = chat.live().await;
        assert!(live.get("compactions").is_none());
        assert_eq!(live["compactionFailure"]["trigger"], "threshold");
        assert_eq!(live["compactionFailure"]["error"], error);
        chat.close().await;
    }
}

#[tokio::test]
async fn stops_through_abort_task_and_conversation_abort_with_background() {
    for stop_task in [true, false] {
        let chat = open(window(2000)).await;
        chat.history().await;
        chat.set_policy(background());
        let reached = Deferred::default();
        chat.faux.summary(gated(&Deferred::default(), summary(), Some(&reached)));
        chat.turn(&text("u4", 100), &text("a4", 100)).await;
        reached.wait().await;
        let task = chat.compaction_tasks().await.remove(0);
        if stop_task {
            chat.harness.abort_task(task.id, &ctx()).await.unwrap();
        } else {
            chat.root.abort(&ctx(), true).await.unwrap();
        }
        assert_eq!(chat.outcome(task.id).await["status"], "aborted");
        assert!(chat.live().await.get("compactions").is_none());
        chat.close().await;
    }
}

#[tokio::test]
async fn waits_for_its_compaction_which_appends_the_summary_before_the_request() {
    let chat = open(window(1000)).await;
    chat.history().await;
    chat.set_policy(blocking());
    // A new section: preparation has a system entry to append, but must not append it before the wait.
    add_section(&chat.setup.registry, "extra", "EXTRA", None);
    let (gate, reached) = (Deferred::default(), Deferred::default());
    chat.faux.summary(gated(&gate, summary(), Some(&reached)));
    chat.faux.agent(answer("a4"));
    let input = chat.submit(&text("u4", 200)).await;
    reached.wait().await;
    let child = chat.compaction_tasks().await.remove(0);
    let generation = chat.run_task().await;
    assert_eq!((child.owner, child.background), (Some(generation), false));
    assert_eq!(child.input.get("reason").and_then(Json::as_str), Some("threshold"));
    let state = serde_json::to_value(chat.harness.get_task(generation, &ctx()).await.unwrap().unwrap().state).unwrap();
    assert_eq!(state["status"], "waiting");
    assert_eq!(state["on"], json!([child.id]));
    assert_eq!(state["checkpoint"]["phase"], "prepare");
    assert_eq!((&state["checkpoint"]["attempt"], &state["checkpoint"]["compacted"]), (&json!(1), &json!(child.id)));
    let expected = json!([{ "taskId": child.id, "trigger": "threshold", "blocking": true, "attempt": 1 }]);
    assert_eq!(chat.live().await["compactions"], expected);
    // Nothing was appended before the wait.
    assert_eq!(chat.kinds().await.last().map(String::as_str), Some("pi.user"));
    gate.resolve();
    assert_eq!(input.wait(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    assert_eq!(chat.outcome(child.id).await["status"], "completed");
    assert_eq!(tail(&chat.kinds().await, 3), ["pi.compaction", "pi.system", "pi.assistant"]);
    let request = chat.faux.last_agent_request();
    assert!(first_user_text(&request).contains("SUMMARY"));
    let systems: Vec<&Message> = request.iter().filter(|message| matches!(message, Message::System(_))).collect();
    assert_eq!(systems.len(), 1);
    let sections = serde_json::to_value(systems[0]).unwrap()["sections"].clone();
    assert_eq!(sections, json!({ "preamble": "You are helpful.", "extra": "<extra>\nEXTRA\n</extra>" }));
    chat.close().await;
}

#[tokio::test]
async fn sends_the_request_once_without_a_second_compaction_when_the_kept_part_is_still_above_the_threshold() {
    let chat = open(window(1000)).await;
    chat.history().await;
    chat.set_policy(CompactionOverrides { keep_recent_tokens: Some(700), ..blocking() });
    chat.faux.summary(summary());
    chat.turn(&text("u4", 400), "a4").await;
    assert_eq!(chat.faux.summary_requests().len(), 1);
    assert_eq!(chat.kinds().await.iter().filter(|kind| *kind == "pi.compaction").count(), 1);
    chat.close().await;
}

#[tokio::test]
async fn sends_the_request_anyway_when_its_compaction_declines() {
    let chat = open(window(1000)).await;
    chat.history().await;
    chat.set_policy(blocking());
    before_compact(&chat, "declines", |_| Ok(Some(CompactionDecision::Decline)));
    chat.turn(&text("u4", 200), "a4").await;
    assert!(!chat.kinds().await.contains(&"pi.compaction".to_owned()));
    assert_eq!(first_user_text(&chat.faux.last_agent_request()), text("u1", 100));
    assert!(chat.live().await.get("compactionFailure").is_none());
    chat.close().await;
}

/// Wait for the unanswered input and check its model error, with no request sent.
async fn fails_without_a_request(chat: &Chat, input: &Submission, requests: usize, detail: &str) {
    let settled = input.wait(&ctx()).await.unwrap();
    assert_eq!((settled.status, settled.reason.as_deref()), (SubmissionStatus::Unanswered, Some("model_error")));
    assert_eq!(settled.detail.map(|detail| detail.to_serde()), Some(json!(detail)));
    assert_eq!(chat.faux.agent_requests().len(), requests);
}

// A failed compaction leaves a context it judged too large; sending it once filled a 1M window and got an empty
// `length` answer on every turn while the failure stayed invisible.
#[tokio::test]
async fn fails_the_run_with_the_compactions_error_without_a_request_when_its_compaction_fails() {
    let chat = open(window(1000)).await;
    chat.history().await;
    chat.set_policy(blocking());
    chat.faux.summary(failure("bad request"));
    let requests = chat.faux.agent_requests().len();
    let input = chat.submit(&text("u4", 200)).await;
    fails_without_a_request(&chat, &input, requests, "Compaction failed: Summarization failed: bad request").await;
    assert_eq!(chat.kinds().await.last().map(String::as_str), Some("pi.user"));
    let live = chat.live().await;
    assert!(live.get("compactions").is_none());
    assert_eq!(live["compactionFailure"]["trigger"], "threshold");
    assert_eq!(live["compactionFailure"]["error"], "Summarization failed: bad request");
    chat.close().await;
}

#[tokio::test]
async fn fails_the_run_when_its_compaction_is_aborted_directly() {
    let chat = open(window(1000)).await;
    chat.history().await;
    chat.set_policy(blocking());
    let reached = Deferred::default();
    chat.faux.summary(gated(&Deferred::default(), summary(), Some(&reached)));
    let requests = chat.faux.agent_requests().len();
    let input = chat.submit(&text("u4", 200)).await;
    reached.wait().await;
    let child = chat.compaction_tasks().await.remove(0);
    chat.harness.abort_task(child.id, &ctx()).await.unwrap();
    fails_without_a_request(&chat, &input, requests, "Compaction was aborted").await;
    assert!(!chat.kinds().await.contains(&"pi.compaction".to_owned()));
    // Aborting is not a failure.
    assert!(chat.live().await.get("compactionFailure").is_none());
    chat.close().await;
}

#[tokio::test]
async fn is_aborted_with_its_generation_by_esc_before_the_generations_abort_handler() {
    let chat = open(window(1000)).await;
    chat.history().await;
    chat.set_policy(blocking());
    let reached = Deferred::default();
    chat.faux.summary(gated(&Deferred::default(), summary(), Some(&reached)));
    let views = views(&chat).await;
    let input = chat.submit(&text("u4", 200)).await;
    reached.wait().await;
    let child = chat.compaction_tasks().await.remove(0);
    chat.root.abort(&ctx(), false).await.unwrap();
    let settled = input.wait(&ctx()).await.unwrap();
    assert_eq!((settled.status, settled.reason.as_deref()), (SubmissionStatus::Unanswered, Some("aborted")));
    assert_eq!(chat.outcome(child.id).await["status"], "aborted");
    views.detach();
    // The compaction's status leaves before the run ends.
    let states = views.live();
    let busy = states.iter().position(|state| state.get("compactions").is_some()).unwrap();
    let after = |test: &dyn Fn(&Value) -> bool| {
        states.iter().enumerate().position(|(index, state)| index > busy && test(state)).unwrap()
    };
    let compaction_end = after(&|state| state.get("compactions").is_none());
    let run_end = after(&|state| state.get("run").is_none());
    assert!(busy > 0);
    assert!(compaction_end < run_end, "{states:?}");
    assert_eq!(states[run_end]["runFailure"]["inputs"], json!([input.id]));
    assert_eq!(states[run_end]["runFailure"]["reason"], "aborted");
    chat.close().await;
}

#[tokio::test]
async fn wins_over_a_background_compaction_still_in_flight_which_then_settles_stale() {
    let chat = open(window(2000)).await;
    chat.history().await;
    chat.set_policy(background());
    let (gate, reached) = (Deferred::default(), Deferred::default());
    chat.faux.summary(gated(&gate, answer("BACKGROUND"), Some(&reached)));
    chat.turn(&text("u4", 100), &text("a4", 100)).await;
    reached.wait().await;
    let background = chat.compaction_tasks().await.remove(0);
    chat.faux.summary(answer("BLOCKING"));
    chat.turn(&text("u5", 1000), "a5").await;
    assert!(chat.first_text().await.contains("BLOCKING"));
    gate.resolve();
    let placed = chat.placed(background.id).await;
    let status = placed.status(&ctx()).await.unwrap();
    assert_eq!((status.status, status.reason.as_deref()), (SubmissionStatus::Unanswered, Some("stale")));
    chat.close().await;
}
