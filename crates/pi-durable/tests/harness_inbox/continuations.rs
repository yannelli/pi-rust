//! The remaining upstream inbox cases of continuations, steers, terminating rounds, handoffs, and aborted runs.

use pi_durable::TaskId;

use super::boundaries::{continue_once, control, failing, install_generation_hooks, reason};
use super::*;

#[tokio::test]
async fn keeps_an_on_yield_continuation_across_a_queued_plain_write_with_the_runs_original_input() {
    let setup = setup();
    let (first, reached, release) = gated("first");
    setup.faux.set_responses([first, answer("second")]);
    install_generation_hooks(&setup, "continue", continue_once());
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let input = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    reached.notified().await;
    let write = root.submit(SubmissionDraft::write(queued_note("w")), &ctx()).await.unwrap();
    release.resolve();
    input.wait(&ctx()).await.unwrap();
    let entries = all_entries(&root).await;
    let expected = ["pi.user:a", "pi.assistant:first", "note:w", "pi.user:more", "pi.assistant:second"];
    assert_eq!(transcript(&entries), expected);
    let status = input.status(&ctx()).await.unwrap();
    assert_eq!((status.status, status.answer), (SubmissionStatus::Done, Some(entries.last().unwrap().id)));
    assert_eq!(reason(&write).await.0, SubmissionStatus::Done);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn drops_an_on_yield_continuation_for_a_queued_reset() {
    let setup = setup();
    let (first, reached, release) = gated("first");
    setup.faux.set_responses([first]);
    let always = pi_durable::harness::generation::GenerationHooks {
        on_yield: Some(Arc::new(|_, _, _| Box::pin(async { Ok(Some(pi_ai::UserContent::Text("more".into()))) }))),
        ..Default::default()
    };
    install_generation_hooks(&setup, "continue", always);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let input = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    reached.notified().await;
    root.reset(None, &ctx()).await.unwrap();
    release.resolve();
    input.wait(&ctx()).await.unwrap();
    harness.wait_for_idle(&ctx()).await.unwrap();
    assert_eq!(reason(&input).await.0, SubmissionStatus::Done);
    assert_eq!(transcript(&all_entries(&root).await), ["pi.user:a", "pi.assistant:first", "pi.reset"]);
    assert_eq!(setup.faux.state().call_count, 1);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn adds_every_steer_to_the_run_at_the_post_tools_boundary_with_steering_mode_all() {
    let setup = setup();
    let gate = Deferred::default();
    hold_tool(&setup, gate.clone(), empty_result());
    setup.faux.set_responses([hold_call(), answer("after tools"), answer("follow-up")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    setup.settings.lock().unwrap().steering_mode = Some(QueueMode::All);
    let input = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    tool_running(&harness, &root).await;
    let steer = |text: &str| SubmissionDraft::input(text).with_when_busy(WhenBusy::Steer);
    let s1 = root.submit(steer("s1"), &ctx()).await.unwrap();
    let f = root.submit(SubmissionDraft::input("f"), &ctx()).await.unwrap();
    let s2 = root.submit(steer("s2"), &ctx()).await.unwrap();
    gate.resolve();
    f.wait(&ctx()).await.unwrap();
    let expected = [
        "pi.user:a",
        "pi.assistant",
        "pi.tool-result",
        "pi.user:s1",
        "pi.user:s2",
        "pi.assistant:after tools",
        "pi.user:f",
        "pi.assistant:follow-up",
    ];
    assert_eq!(transcript(&all_entries(&root).await), expected);
    let answers = answers(&root).await;
    for submission in [&input, &s1, &s2] {
        let status = submission.status(&ctx()).await.unwrap();
        assert_eq!((status.status, status.answer), (SubmissionStatus::Done, Some(answers[1])));
    }
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn queues_an_idle_steer_behind_waiting_items_and_places_it_with_the_first_follow_up_in_id_order() {
    let setup = setup();
    let (failing, reached, release) = failing();
    setup.faux.set_responses([failing, answer("both")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let input = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    reached.notified().await;
    let f = root.submit(SubmissionDraft::input("f"), &ctx()).await.unwrap();
    release.resolve();
    input.wait(&ctx()).await.unwrap();
    harness.wait_for_idle(&ctx()).await.unwrap();
    let steer = root.submit(SubmissionDraft::input("s").with_when_busy(WhenBusy::Steer), &ctx()).await.unwrap();
    let settled = steer.wait(&ctx()).await.unwrap();
    let status = f.status(&ctx()).await.unwrap();
    assert_eq!((status.status, status.answer), (SubmissionStatus::Done, settled.answer));
    let transcript = transcript(&all_entries(&root).await);
    assert_eq!(transcript[transcript.len() - 3..], ["pi.user:f", "pi.user:s", "pi.assistant:both"]);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn starts_a_queued_follow_up_after_a_terminating_round() {
    let setup = setup();
    let gate = Deferred::default();
    hold_tool(&setup, gate.clone(), control(ToolControl { terminate: Some(true), ..ToolControl::default() }));
    setup.faux.set_responses([hold_call(), answer("follow-up")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let input = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    tool_running(&harness, &root).await;
    let f = root.submit(SubmissionDraft::input("f"), &ctx()).await.unwrap();
    gate.resolve();
    f.wait(&ctx()).await.unwrap();
    let entries = all_entries(&root).await;
    let calling = entries.iter().find(|entry| entry.kind == "pi.assistant").unwrap();
    let status = input.status(&ctx()).await.unwrap();
    assert_eq!((status.status, status.answer), (SubmissionStatus::Done, Some(calling.id)));
    let expected = ["pi.user:a", "pi.assistant", "pi.tool-result", "pi.user:f", "pi.assistant:follow-up"];
    assert_eq!(transcript(&entries), expected);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn writes_the_last_handoff_in_call_order_and_then_runs_queued_follow_ups_in_the_new_context() {
    let setup = setup();
    let (first_gate, second_gate) = (Deferred::default(), Deferred::default());
    hold_tool(
        &setup,
        first_gate.clone(),
        control(ToolControl { handoff: Some("one".into()), ..ToolControl::default() }),
    );
    let held = second_gate.clone();
    let later = define_tool("later", "Finishes first", json!({ "type": "object" }), move |_, _, _| {
        let held = held.clone();
        async move {
            held.wait().await;
            Ok(control(ToolControl { handoff: Some("two".into()), ..ToolControl::default() }))
        }
    });
    setup.registry.install(Extension::new("tool:later").tool(later)).unwrap();
    let calls = vec![faux_tool_call_with_id("hold", json!({}), "c1"), faux_tool_call_with_id("later", json!({}), "c2")];
    let round: FauxResponseStep =
        faux_assistant_message(calls, FauxMessageOptions::stop_reason(StopReason::ToolUse)).into();
    setup.faux.set_responses([round, answer("follow-up")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let input = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    tool_running(&harness, &root).await;
    let f = root.submit(SubmissionDraft::input("f"), &ctx()).await.unwrap();
    second_gate.resolve();
    wait_for(|| {
        let (harness, root) = (harness.clone(), root.clone());
        async move {
            let live = harness.snapshot(&LIVE_DOC, root.id, &ctx()).await.unwrap();
            live.and_then(|live| live.get("tools")?.as_array()?.get(1)?.get("status").cloned())
                == Some(Json::from("done"))
        }
    })
    .await;
    first_gate.resolve();
    f.wait(&ctx()).await.unwrap();
    assert_eq!(input.status(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    let transcript = transcript(&all_entries(&root).await);
    let expected = ["pi.tool-result", "pi.reset:two", "pi.user:f", "pi.assistant:follow-up"];
    assert_eq!(transcript[transcript.len() - 4..], expected);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn leaves_the_inbox_alone_when_the_runs_task_is_aborted() {
    let setup = setup();
    let (first, reached, _release) = gated("never");
    setup.faux.set_responses([first]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let input = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    reached.notified().await;
    let f = root.submit(SubmissionDraft::input("f"), &ctx()).await.unwrap();
    let live = harness.snapshot(&LIVE_DOC, root.id, &ctx()).await.unwrap().unwrap().to_serde();
    let task_id: TaskId = serde_json::from_value(live["run"]["taskId"].clone()).unwrap();
    harness.abort_task(task_id, &ctx()).await.unwrap();
    let settled = input.wait(&ctx()).await.unwrap();
    assert_eq!((settled.status, settled.reason.as_deref()), (SubmissionStatus::Unanswered, Some("aborted")));
    harness.wait_for_idle(&ctx()).await.unwrap();
    assert_eq!(f.status(&ctx()).await.unwrap().status, SubmissionStatus::Queued);
    assert_eq!(inbox(&harness, &root).await, [(f.id, "followUp".to_owned())]);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn returns_a_queued_submission_for_its_repeated_request_id_without_a_second_item() {
    let setup = setup();
    let (first, reached, _release) = gated("first");
    setup.faux.set_responses([first]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    reached.notified().await;
    let queued = root.submit(SubmissionDraft::input("f").with_request_id("r"), &ctx()).await.unwrap();
    let again = root.submit(SubmissionDraft::input("f").with_request_id("r"), &ctx()).await.unwrap();
    assert_eq!(again.id, queued.id);
    assert_eq!(inbox(&harness, &root).await, [(queued.id, "followUp".to_owned())]);
    harness.close(&ctx()).await.unwrap();
}
