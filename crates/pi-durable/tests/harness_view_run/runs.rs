//! Upstream `harness-view-run.test.ts`: queued inputs, run boundaries, a steer, a reset, and an aborted partial.

use pi_ai::StopReason;
use pi_durable::ConversationOwnership;
use pi_durable::testing::faux::{FauxMessageOptions, FauxResponseStep, faux_assistant_message, faux_text};

use super::support::chat::{call, no_parameters};
use super::*;

/// A faux answer held until `release` resolves or its request is cancelled.
fn held(release: Deferred, text: &'static str) -> FauxResponseStep {
    FauxResponseStep::async_factory(move |_, options, _, _| {
        let (release, signal) = (release.clone(), options.signal.clone());
        Box::pin(async move {
            tokio::select! {
                () = release.wait() => Ok(faux_assistant_message(vec![faux_text(text)], FauxMessageOptions::default())),
                error = async { signal.unwrap().cancelled().await } => Err(error.to_string()),
            }
        })
    })
}

fn inbox(view: &Value) -> Vec<Value> {
    view["docs"]["pi.inbox"]["items"].as_array().cloned().unwrap_or_default()
}

#[tokio::test]
async fn shows_queued_inputs_in_the_inbox_a_retry_backoff_and_the_next_run() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    let release = Deferred::default();
    let options = FauxMessageOptions {
        error_message: Some("503 Service Unavailable".into()),
        ..FauxMessageOptions::stop_reason(StopReason::Error)
    };
    let error = faux_assistant_message(Vec::new(), options);
    setup.faux.set_responses([held(release.clone(), "first"), error.into(), answer("second")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    setup.settings.lock().unwrap().retry = Some(RetryOverrides {
        enabled: Some(true),
        max_retries: Some(1),
        base_delay_ms: Some(1),
        ..Default::default()
    });
    let history = view_history(&harness, root.id).await;
    let first = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    let follow_up = root.submit(SubmissionDraft::input("f"), &ctx()).await.unwrap();
    let queued = inbox(history.views().last().unwrap());
    assert_eq!(queued.len(), 1, "{queued:?}");
    assert_eq!((&queued[0]["id"], &queued[0]["mode"]), (&json!(follow_up.id), &json!("followUp")));
    release.resolve();
    follow_up.wait(&ctx()).await.unwrap();
    history.detach();
    let states = history.live();
    assert!(states.iter().any(|state| state["generation"]["retry"]["error"] == "503 Service Unavailable"));
    let retried = states.iter().position(|state| state["generation"].get("retry").is_some()).unwrap();
    assert!(states[retried..].iter().any(|state| state["generation"]["attempt"] == 2));
    let mut runs: Vec<Value> = Vec::new();
    for (index, state) in states.iter().enumerate() {
        let inputs = &state["run"]["inputs"];
        let previous = index.checked_sub(1).map(|before| &states[before]["run"]["inputs"]);
        if !inputs.is_null() && previous != Some(inputs) {
            runs.push(inputs.clone());
        }
    }
    assert_eq!(runs.first(), Some(&json!([first.id])));
    assert_eq!(runs.last(), Some(&json!([follow_up.id])));
    assert_eq!(inbox(history.views().last().unwrap()), Vec::<Value>::new());
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn ends_a_run_and_starts_the_queued_follow_ups_in_one_value() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    let release = Deferred::default();
    setup.faux.set_responses([held(release.clone(), "first"), answer("second")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let input = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    let follow_up = root.submit(SubmissionDraft::input("f"), &ctx()).await.unwrap();
    let history = view_history(&harness, root.id).await;
    release.resolve();
    follow_up.wait(&ctx()).await.unwrap();
    history.detach();
    let (states, views) = (history.live(), history.views());
    let boundary = states.iter().position(|state| state["run"]["inputs"][0] == json!(follow_up.id)).unwrap();
    assert_eq!(states[boundary - 1]["run"]["inputs"], json!([input.id]));
    let (before, after) = (&views[boundary - 1], &views[boundary]);
    let appended: Vec<(Value, Option<String>)> = view_entries(after)[view_entries(before).len()..]
        .iter()
        .map(|entry| (entry["kind"].clone(), text_in(&entry["model"][0])))
        .collect();
    let expected = [(json!("pi.assistant"), Some("first".to_owned())), (json!("pi.user"), Some("f".to_owned()))];
    assert_eq!(appended, expected);
    assert_eq!(inbox(after), Vec::<Value>::new());
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn keeps_a_steer_in_the_current_run_shows_nothing_for_other_conversations_and_appends_a_reset() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    let gate = Deferred::default();
    let tool_gate = gate.clone();
    add_tool(
        &setup,
        define_tool("hold", "Waits", no_parameters(), move |_, _, _| {
            let gate = tool_gate.clone();
            async move {
                gate.wait().await;
                Ok(ToolExecutionResult::default())
            }
        }),
    );
    setup.faux.set_responses([call("hold", "c1"), answer("done")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let ownerless = CreateOptions { ownership: ConversationOwnership::Ownerless, ..CreateOptions::default() };
    let other = harness.create_conversation(ownerless, &ctx()).await.unwrap();
    let history = view_history(&harness, root.id).await;
    let input = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    wait_for(|| {
        let running = slots_of(&history.live(), "c1").iter().flatten().any(|slot| slot["status"] == "running");
        async move { running }
    })
    .await;
    let steer = root.submit(SubmissionDraft::input("s").with_when_busy(WhenBusy::Steer), &ctx()).await.unwrap();
    let count = history.views().len();
    append_note(&other).await;
    assert_eq!(history.views().len(), count);
    gate.resolve();
    input.wait(&ctx()).await.unwrap();
    root.reset(None, &ctx()).await.unwrap();
    history.detach();
    let states = history.live();
    // One run: the steer joined it.
    let mut runs: Vec<Value> = Vec::new();
    for first in states.iter().map(|state| &state["run"]["inputs"][0]).filter(|first| !first.is_null()) {
        if !runs.contains(first) {
            runs.push(first.clone());
        }
    }
    assert_eq!(runs, [json!(input.id)]);
    let joined = |state: &Value| state["run"]["inputs"].as_array().is_some_and(|ids| ids.contains(&json!(steer.id)));
    assert!(states.iter().any(joined));
    // The reset is appended in place, with no run.
    assert_eq!(last_entry(history.views().last().unwrap())["kind"], "pi.reset");
    assert!(states.last().unwrap().get("run").is_none());
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn converts_the_streamed_partial_into_one_aborted_answer_entry_when_the_run_is_aborted() {
    let setup = paced(100.0);
    setup.faux.set_responses([answer(&"x".repeat(400))]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let history = view_history(&harness, root.id).await;
    let submission = root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap();
    wait_for(|| {
        let streaming = history.live().iter().any(|state| state["generation"].get("message").is_some());
        async move { streaming }
    })
    .await;
    harness.abort_task(run_task(&harness, &root).await, &ctx()).await.unwrap();
    submission.wait(&ctx()).await.unwrap();
    history.detach();
    let (states, views) = (history.live(), history.views());
    let ended = (1..states.len()).find(|&index| states[index].get("run").is_none()).unwrap();
    assert!(states[ended - 1]["generation"].get("message").is_some(), "{}", states[ended - 1]);
    let appended = &view_entries(&views[ended])[view_entries(&views[ended - 1]).len()..];
    let kinds: Vec<&Value> = appended.iter().map(|entry| &entry["kind"]).collect();
    assert_eq!(kinds, [&json!("pi.assistant")]);
    let message = &appended[0]["model"][0];
    assert_eq!((&message["role"], &message["stopReason"]), (&json!("assistant"), &json!("aborted")));
    assert_eq!(states[ended]["runFailure"]["reason"], "aborted");
    harness.close(&ctx()).await.unwrap();
}
