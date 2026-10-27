//! Upstream inbox cases of tool handoffs: the run continues in the new context (upstream 38ff99177).

use std::sync::atomic::{AtomicUsize, Ordering};

use super::boundaries::control;
use super::*;

/// A faux response that records the request's messages as `role:text` and answers `text`.
fn record_request(requests: &Arc<Mutex<Vec<Vec<String>>>>, text: &str) -> FauxResponseStep {
    let (sink, text) = (requests.clone(), text.to_owned());
    FauxResponseStep::factory(move |transcript, _, _, _| {
        let roles = transcript.messages.iter().map(|message| {
            let role = match message {
                Message::System(_) => "system",
                Message::User(_) => "user",
                Message::Assistant(_) => "assistant",
                _ => "other",
            };
            format!("{role}:{}", text_of(Some(message)).unwrap_or_default())
        });
        sink.lock().unwrap().push(roles.collect());
        faux_assistant_message(vec![faux_text(&text)], FauxMessageOptions::default())
    })
}

fn handoff(text: &str) -> ToolExecutionResult {
    control(ToolControl { handoff: Some(text.into()), ..ToolControl::default() })
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

#[tokio::test]
async fn continues_the_run_in_a_new_context_when_a_tool_requests_a_handoff() {
    let setup = setup();
    let gate = Deferred::default();
    gate.resolve();
    hold_tool(&setup, gate, handoff("continue here"));
    let requests = Arc::default();
    setup.faux.set_responses([hold_call(), record_request(&requests, "after handoff")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let settled = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let entries = all_entries(&root).await;
    assert_eq!((settled.status, settled.answer), (SubmissionStatus::Done, Some(entries.last().unwrap().id)));
    let expected =
        ["pi.user:a", "pi.assistant", "pi.tool-result", "pi.reset:continue here", "pi.assistant:after handoff"];
    assert_eq!(transcript(&entries), expected);
    let reset = entries.iter().find(|entry| entry.kind == "pi.reset").unwrap();
    assert_eq!(reset.head, Some(reset.id));
    assert_eq!(*requests.lock().unwrap(), [strings(&["system:", "user:continue here"])]);
    assert_eq!(setup.faux.state().call_count, 2);
    assert_eq!(harness.snapshot(&LIVE_DOC, root.id, &ctx()).await.unwrap(), Some(Json::from(json!({}))));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn places_queued_writes_and_steers_after_the_handoff_text_and_adds_the_steers_to_the_continuing_run() {
    let setup = setup();
    let gate = Deferred::default();
    hold_tool(&setup, gate.clone(), handoff("continue here"));
    let requests = Arc::default();
    setup.faux.set_responses([hold_call(), record_request(&requests, "after handoff")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let input = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    tool_running(&harness, &root).await;
    let steer = root.submit(SubmissionDraft::input("s").with_when_busy(WhenBusy::Steer), &ctx()).await.unwrap();
    let write = root.submit(SubmissionDraft::write(queued_note("w")), &ctx()).await.unwrap();
    gate.resolve();
    input.wait(&ctx()).await.unwrap();
    let entries = all_entries(&root).await;
    let expected = [
        "pi.user:a",
        "pi.assistant",
        "pi.tool-result",
        "pi.reset:continue here",
        "note:w",
        "pi.user:s",
        "pi.assistant:after handoff",
    ];
    assert_eq!(transcript(&entries), expected);
    assert_eq!(*requests.lock().unwrap(), [strings(&["system:", "user:continue here", "user:w", "user:s"])]);
    let last = entries.last().unwrap().id;
    for submission in [&input, &steer] {
        let status = submission.status(&ctx()).await.unwrap();
        assert_eq!((status.status, status.answer), (SubmissionStatus::Done, Some(last)));
    }
    assert_eq!(write.status(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn holds_a_follow_up_queued_across_a_handoff_until_the_continuing_run_s_final_boundary() {
    let setup = setup();
    let gate = Deferred::default();
    hold_tool(&setup, gate.clone(), handoff("continue here"));
    let (after, reached, release) = gated("after handoff");
    setup.faux.set_responses([hold_call(), after, answer("follow-up")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let input = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    tool_running(&harness, &root).await;
    let follow_up = root.submit(SubmissionDraft::input("f"), &ctx()).await.unwrap();
    let reaching = reached.notified();
    gate.resolve();
    reaching.await;
    assert_eq!(input.status(&ctx()).await.unwrap().status, SubmissionStatus::Placed);
    assert_eq!(inbox(&harness, &root).await, [(follow_up.id, "followUp".to_owned())]);
    release.resolve();
    follow_up.wait(&ctx()).await.unwrap();
    let entries = all_entries(&root).await;
    let expected = [
        "pi.user:a",
        "pi.assistant",
        "pi.tool-result",
        "pi.reset:continue here",
        "pi.assistant:after handoff",
        "pi.user:f",
        "pi.assistant:follow-up",
    ];
    assert_eq!(transcript(&entries), expected);
    let answers = answers(&root).await;
    assert_eq!(input.status(&ctx()).await.unwrap().answer, Some(answers[1]));
    assert_eq!(follow_up.status(&ctx()).await.unwrap().answer, Some(answers[2]));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn ends_a_handed_off_run_at_a_queued_reset_and_runs_earlier_follow_ups_after_it() {
    let setup = setup();
    let gate = Deferred::default();
    hold_tool(&setup, gate.clone(), handoff("continue here"));
    let requests = Arc::default();
    setup.faux.set_responses([hold_call(), record_request(&requests, "fresh")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let input = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    tool_running(&harness, &root).await;
    let follow_up = root.submit(SubmissionDraft::input("f"), &ctx()).await.unwrap();
    root.reset(None, &ctx()).await.unwrap();
    gate.resolve();
    follow_up.wait(&ctx()).await.unwrap();
    let status = input.status(&ctx()).await.unwrap();
    assert_eq!((status.status, status.reason.as_deref()), (SubmissionStatus::Unanswered, Some("reset")));
    let expected = [
        "pi.user:a",
        "pi.assistant",
        "pi.tool-result",
        "pi.reset:continue here",
        "pi.reset",
        "pi.user:f",
        "pi.assistant:fresh",
    ];
    assert_eq!(transcript(&all_entries(&root).await), expected);
    assert_eq!(*requests.lock().unwrap(), [strings(&["system:", "user:f"])]);
    assert_eq!(setup.faux.state().call_count, 2);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn hands_off_again_in_a_later_round_of_the_same_run() {
    let setup = setup();
    let handoffs = Arc::new(AtomicUsize::new(0));
    let tool = define_tool("hold", "Hands off", json!({ "type": "object" }), move |_, _, _| {
        let count = handoffs.fetch_add(1, Ordering::SeqCst) + 1;
        async move { Ok(handoff(&format!("h{count}"))) }
    });
    setup.registry.install(Extension::new("tool:hold").tool(tool)).unwrap();
    setup.faux.set_responses([hold_call(), hold_call(), answer("done")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let settled = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let entries = all_entries(&root).await;
    let expected = [
        "pi.user:a",
        "pi.assistant",
        "pi.tool-result",
        "pi.reset:h1",
        "pi.assistant",
        "pi.tool-result",
        "pi.reset:h2",
        "pi.assistant:done",
    ];
    assert_eq!(transcript(&entries), expected);
    assert_eq!((settled.status, settled.answer), (SubmissionStatus::Done, Some(entries.last().unwrap().id)));
    let messages = root.context(&ctx(), None).await.unwrap().messages;
    let roles: Vec<String> = messages
        .iter()
        .map(|message| {
            let role = match message {
                Message::System(_) => "system",
                Message::User(_) => "user",
                Message::Assistant(_) => "assistant",
                _ => "other",
            };
            format!("{role}:{}", text_of(Some(message)).unwrap_or_default())
        })
        .collect();
    assert_eq!(roles, strings(&["system:", "user:h2", "assistant:done"]));
    harness.close(&ctx()).await.unwrap();
}
