//! Upstream "a generation interrupted while the model answers": a close, a sleep, a tool wait, and preparation.

use pi_ai::StopReason;

use super::support::Deferred;
use super::support::chat::{add_tool, call, no_parameters};
use super::support::task_support::aborted;
use super::*;

#[tokio::test]
async fn resends_after_a_clean_close_a_close_is_not_a_crash() {
    let storage = Arc::new(ControlledStorage::default());
    let first = streaming();
    retry(&first, false, 0, 0);
    let (harness, root) = open(&storage, &first).await;
    harness.resume().unwrap();
    let id = root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap().id;
    wait_for(|| async { live(&harness).await["generation"].get("message").is_some() }).await;
    let task_id = run_task_id(&harness).await;
    harness.close(&ctx()).await.unwrap();

    let setup = chat_setup(Default::default());
    retry(&setup, false, 0, 0);
    setup.faux.set_responses([answer("answer")]);
    let (reopened, _) = open(&storage, &setup).await;
    assert_eq!(reopened.get_task(task_id, &ctx()).await.unwrap().unwrap().restarts, None);
    reopened.resume().unwrap();
    assert_eq!(settled(&reopened, id).await.status, SubmissionStatus::Done);
    reopened.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn does_not_charge_a_generation_that_sleeps_in_a_retry_or_waits_for_its_tools() {
    let storage = Arc::new(ControlledStorage::default());
    let setup = chat_setup(Default::default());
    retry(&setup, true, 1, 60_000);
    let (gate, started) = (Deferred::default(), Deferred::default());
    let (held, reached) = (gate.clone(), started.clone());
    let hold = define_tool("hold", "Waits", no_parameters(), move |_, _, context| {
        let (held, reached) = (held.clone(), reached.clone());
        async move {
            reached.resolve();
            // A crash aborts this process's call, which otherwise holds the dead Harness's close open.
            tokio::select! {
                _ = held.wait() => {}
                _ = aborted(context.abort_signal().unwrap()) => {}
            }
            Ok(ToolExecutionResult::default())
        }
    })
    .with_replay(ToolReplay::Safe);
    add_tool(&setup, hold.clone());
    let unavailable = FauxMessageOptions {
        stop_reason: Some(StopReason::Error),
        error_message: Some("503 Service Unavailable".into()),
        ..FauxMessageOptions::default()
    };
    setup.faux.set_responses([faux_assistant_message(Vec::new(), unavailable)]);
    let (harness, root) = open(&storage, &setup).await;
    harness.resume().unwrap();
    let id = root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap().id;
    let task_id = run_task_id(&harness).await;
    let sleeping =
        || async { harness.get_task(task_id, &ctx()).await.unwrap().unwrap().status() == TaskStatus::Sleeping };
    wait_for(sleeping).await;
    kill(&harness).await;

    // Asleep in its retry: not running, so not cut off.
    let later = chat_setup(Default::default());
    retry(&later, true, 1, 60_000);
    *later.now_offset_ms.lock().unwrap() = 120_000.0;
    add_tool(&later, hold);
    later.faux.set_responses([call("hold", "c1"), answer("done")]);
    let (harness, _) = open(&storage, &later).await;
    assert_eq!(harness.get_task(task_id, &ctx()).await.unwrap().unwrap().restarts, None);
    harness.resume().unwrap();
    // Waiting for its tool round: not running either.
    started.wait().await;
    let waiting = serde_json::to_value(harness.get_task(task_id, &ctx()).await.unwrap().unwrap().state).unwrap();
    assert_eq!((&waiting["status"], &waiting["checkpoint"]["phase"]), (&json!("waiting"), &json!("tools")));
    kill(&harness).await;

    let (harness, _) = open(&storage, &later).await;
    assert_eq!(harness.get_task(task_id, &ctx()).await.unwrap().unwrap().restarts, None);
    gate.resolve();
    harness.resume().unwrap();
    assert_eq!(settled(&harness, id).await.status, SubmissionStatus::Done);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn ends_a_run_whose_generation_is_cut_off_restart_limit_times_outside_a_request() {
    let storage = Arc::new(ControlledStorage::default());
    let setup = chat_setup(Default::default());
    restart_limit(&setup, 1.0);
    let reached = Deferred::default();
    let signal = reached.clone();
    // Preparation never finishes, as one that exhausts the host's memory would not.
    let preamble = section(
        "preamble",
        move |_, context| {
            let signal = signal.clone();
            async move {
                signal.resolve();
                aborted(context.abort_signal().unwrap()).await;
                Ok(None)
            }
        },
        None,
    );
    setup.registry.install(Extension::new("section:preamble").section(preamble)).unwrap();
    let (harness, root) = open(&storage, &setup).await;
    harness.resume().unwrap();
    let id = root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap().id;
    reached.wait().await;
    let task_id = run_task_id(&harness).await;
    kill(&harness).await;

    let (harness, _) = open(&storage, &setup).await;
    harness.resume().unwrap();
    let text = "The generation was interrupted once in its prepare phase";
    let record = settled(&harness, id).await;
    assert_eq!((record.status, record.reason.as_deref()), (SubmissionStatus::Unanswered, Some("restart_limit")));
    assert_eq!(detail(&record), Some(json!(text)));
    let failure = json!({ "taskId": task_id, "inputs": [id], "reason": "restart_limit", "detail": text });
    assert_eq!(live(&harness).await, json!({ "runFailure": failure }));
    wait_for(|| async { !setup.infos.lock().unwrap().is_empty() }).await;
    let infos = serde_json::to_value(&*setup.infos.lock().unwrap()).unwrap();
    let interrupted = |info: &&Value| info["type"] == "task_interrupted" && info["reason"] == "restart_limit";
    assert_eq!(infos.as_array().unwrap().iter().find(interrupted).unwrap()["restarts"], 1, "{infos}");
    harness.close(&ctx()).await.unwrap();
}
