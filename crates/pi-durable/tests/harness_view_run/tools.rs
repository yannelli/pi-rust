//! Upstream `harness-view-run.test.ts`: a tool's sliding output window, a call never run, and a tool aborted with its
//! generation.

use pi_durable::DurableError;

use super::support::chat::no_parameters;
use super::*;

#[tokio::test]
async fn rebuilds_a_sliding_tail_window_from_output_trims_and_appends() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    let gate = Deferred::default();
    let tool_gate = gate.clone();
    let tool = define_tool("tail", "Prints lines", no_parameters(), move |_, api, _| {
        let gate = tool_gate.clone();
        async move {
            for line in 0..6 {
                api.output(&format!("line {line}\n"))?;
                tokio::time::sleep(Duration::from_millis(120)).await;
            }
            gate.wait().await;
            Ok(ToolExecutionResult::default())
        }
    })
    .with_output_limits(ToolOutputLimits { max_lines: Some(3), retain: Some(Retain::Tail), ..Default::default() });
    add_tool(&setup, tool);
    setup.faux.set_responses([calls(&[("tail", json!({}), "c1")]), answer("done")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let committed = committed_live(&harness, |live| live["tools"][0].get("output").cloned());
    let (acquired, frames, watch) = record(&root).await;
    let submission = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap();
    wait_for(|| {
        let last = committed.lock().unwrap().last().cloned();
        let done = last.is_some_and(|output| output.as_str().unwrap_or_default().ends_with("line 5\n"));
        async move { done }
    })
    .await;
    gate.resolve();
    submission.wait(&ctx()).await.unwrap();
    drained().await;
    watch.stop().await;
    let frames = frames.lock().unwrap().clone();
    replay(&acquired, &frames);
    let mut outputs = committed.lock().unwrap().clone();
    outputs.dedup();
    let mut shown: Vec<Value> = Vec::new();
    for (value, _) in &frames {
        let output = &live_of(value)["tools"][0]["output"];
        if !output.is_null() && shown.last() != Some(output) {
            shown.push(output.clone());
        }
    }
    assert_eq!(shown, outputs);
    assert!(frames.iter().any(|(_, ops)| ops.iter().any(|op| op.verb() == "t")));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn ends_a_call_that_never_runs_with_its_result_at_once_and_a_tool_aborted_with_its_generation() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    let wait =
        define_tool("wait", "Waits until aborted", no_parameters(), |_, _, context: pi_chord::Context| async move {
            let error = context.abort_signal().unwrap().cancelled().await;
            Err(DurableError::from(error))
        });
    add_tool(&setup, wait);
    setup.faux.set_responses([calls(&[("ghost", json!({}), "c1"), ("wait", json!({}), "c2")])]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let history = view_history(&harness, root.id).await;
    let submission = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap();
    wait_for(|| {
        let running = slots_of(&history.live(), "c2").iter().flatten().any(|slot| slot["status"] == "running");
        async move { running }
    })
    .await;
    // Aborting the generation aborts its round: the tool ends with its aborted result first (spec section 8.5).
    harness.abort_task(run_task(&harness, &root).await, &ctx()).await.unwrap();
    submission.wait(&ctx()).await.unwrap();
    history.detach();
    let (states, views) = (history.live(), history.views());
    // The call not offered is listed done, with its result entry, in the commit of its calling answer.
    let ghosts = slots_of(&states, "c1");
    let listed = ghosts.iter().position(Option::is_some).unwrap();
    let ghost = ghosts[listed].as_ref().unwrap();
    assert_eq!(ghost["status"], "done");
    let listing = view_entries(&views[listed]);
    let kinds: Vec<&str> = listing.iter().map(|entry| entry["kind"].as_str().unwrap_or_default()).collect();
    assert_eq!(kinds[kinds.len() - 2..], ["pi.assistant", "pi.tool-result"]);
    assert_eq!(ghost["entry"], listing.last().unwrap()["id"]);
    // The aborted tool ends with its result entry, then the run ends without an answer.
    let waits = slots_of(&states, "c2");
    let done = waits.iter().position(|slot| slot.as_ref().is_some_and(|slot| slot["status"] == "done")).unwrap();
    assert_eq!(waits[done].as_ref().unwrap()["entry"], last_entry(&views[done])["id"]);
    let failure = &states.last().unwrap()["runFailure"];
    assert_eq!((&failure["inputs"], &failure["reason"]), (&json!([submission.id]), &json!("aborted")));
    harness.close(&ctx()).await.unwrap();
}
