//! Upstream "crash edges" of deferred responses: an interrupted poll, and an abort handler cut off at the limit.

use std::sync::atomic::{AtomicUsize, Ordering};

use pi_ai::StopReason;
use pi_durable::models::DeferredOption;
use pi_durable::testing::faux::FauxDeferredOptions;

use super::support::Deferred;
use super::support::models_support::{CancelFn, FetchFn, WrappedModels};
use super::support::task_support::aborted;
use super::*;

const STOPPED_FETCHING: &str = "The host stopped while the deferred answer was fetched, on attempt 1";

fn deferred_base(poll_after_ms: u64) -> ChatSetup {
    let deferred = FauxDeferredOptions { pending_fetches: 0, poll_after_ms: Some(poll_after_ms) };
    let setup = chat_setup(RegisterFauxProviderOptions { deferred: Some(deferred), ..Default::default() });
    let stream = ConversationStreamOptions { deferred: Some(DeferredOption::Enabled(true)), ..Default::default() };
    setup.settings.lock().unwrap().stream = Some(stream);
    setup
}

/// A deferred model whose first fetch is in flight until the Harness stops; later fetches answer. `fetching`
/// resolves when the first fetch began.
fn deferred_setup(poll_after_ms: u64) -> (ChatSetup, Deferred, Arc<AtomicUsize>) {
    let base = deferred_base(poll_after_ms);
    base.faux.set_responses([answer("deferred answer")]);
    let (fetching, fetches) = (Deferred::default(), Arc::new(AtomicUsize::new(0)));
    let (began, counted) = (fetching.clone(), fetches.clone());
    let fetch: FetchFn = Arc::new(move |inner, model, handle, options| {
        if counted.fetch_add(1, Ordering::SeqCst) > 0 {
            return inner.fetch_deferred(model, handle, options);
        }
        began.resolve();
        Box::pin(async move {
            aborted(options.signal.unwrap()).await;
            let stopped =
                FauxMessageOptions { stop_reason: Some(StopReason::Aborted), ..FauxMessageOptions::default() };
            faux_assistant_message(Vec::new(), stopped)
        })
    });
    let models = WrappedModels::fetching(base.models.clone(), fetch);
    (ChatSetup { models, ..base }, fetching, fetches)
}

/// Submit to a fresh chat, let its deferred response be fetched, and crash during that fetch.
async fn crash_while_polling(
    storage: &Arc<ControlledStorage>,
    setup: &ChatSetup,
    fetching: &Deferred,
) -> (SubmissionId, TaskId) {
    let (harness, root) = open(storage, setup).await;
    harness.resume().unwrap();
    let id = root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap().id;
    fetching.wait().await;
    let task_id = run_task_id(&harness).await;
    let state = serde_json::to_value(harness.get_task(task_id, &ctx()).await.unwrap().unwrap().state).unwrap();
    assert_eq!((&state["status"], &state["checkpoint"]["phase"]), (&json!("running"), &json!("poll")));
    kill(&harness).await;
    (id, task_id)
}

#[tokio::test]
async fn charges_a_crash_during_a_polls_fetch_then_polls_the_same_deferred_response_again() {
    let storage = Arc::new(ControlledStorage::default());
    let (setup, fetching, fetches) = deferred_setup(10);
    retry(&setup, true, 2, 1);
    let (id, task_id) = crash_while_polling(&storage, &setup, &fetching).await;

    let (harness, root) = open(&storage, &setup).await;
    let values = live_values(&harness);
    assert_eq!(harness.get_task(task_id, &ctx()).await.unwrap().unwrap().restarts, Some(1));
    harness.resume().unwrap();
    assert_eq!(settled(&harness, id).await.status, SubmissionStatus::Done);
    let values = values.get();
    let retry = values.iter().find(|value| value["generation"].get("retry").is_some()).unwrap()["generation"].clone();
    assert_eq!((&retry["attempt"], &retry["retry"]["error"]), (&json!(1), &json!(STOPPED_FETCHING)));
    assert!(retry["retry"]["at"].is_number() && retry.as_object().unwrap().len() == 2, "{retry}");
    assert_eq!(retry["retry"].as_object().unwrap().len(), 2, "{retry}");
    let polled_again =
        |value: &Value| value["generation"]["attempt"] == 2 && value["generation"].get("deferred").is_some();
    assert!(values.iter().any(polled_again));
    // One request, polled twice: the provider's job is neither dropped nor submitted again.
    assert_eq!(setup.faux.state().call_count, 1);
    assert_eq!(fetches.load(Ordering::SeqCst), 2);
    assert!(setup.faux.state().cancelled_deferred.is_empty());
    let entries = all_entries(&root).await;
    assert_eq!(
        text_of(entries.last().unwrap().model.as_ref().and_then(|model| model.first())).as_deref(),
        Some("deferred answer")
    );
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn cancels_the_deferred_response_when_a_crash_during_its_poll_ends_the_run() {
    let storage = Arc::new(ControlledStorage::default());
    let (setup, fetching, _) = deferred_setup(10);
    retry(&setup, false, 0, 0);
    let (id, task_id) = crash_while_polling(&storage, &setup, &fetching).await;
    let (harness, _) = open(&storage, &setup).await;
    harness.resume().unwrap();
    let record = settled(&harness, id).await;
    assert_eq!((record.status, record.reason.as_deref()), (SubmissionStatus::Unanswered, Some("interrupted")));
    assert_eq!(detail(&record), Some(json!(STOPPED_FETCHING)));
    let failure = json!({ "taskId": task_id, "inputs": [id], "reason": "interrupted", "detail": STOPPED_FETCHING });
    assert_eq!(live(&harness).await, json!({ "runFailure": failure }));
    assert_eq!(setup.faux.state().cancelled_deferred.len(), 1);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn charges_an_interrupted_poll_even_when_the_clock_moved_backwards() {
    let storage = Arc::new(ControlledStorage::default());
    let (setup, fetching, _) = deferred_setup(10);
    *setup.now.lock().unwrap() = Some(1000.0);
    retry(&setup, false, 0, 0);
    let (first, root) = open(&storage, &setup).await;
    first.resume().unwrap();
    let id = root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap().id;
    wait_for(|| async { live(&first).await["generation"].get("deferred").is_some() }).await;
    *setup.now.lock().unwrap() = Some(1010.0);
    first.resume().unwrap();
    fetching.wait().await;
    kill(&first).await;
    // A replacement host whose clock is behind: the poll looks not due yet, but its fetch was cut off.
    *setup.now.lock().unwrap() = Some(1000.0);
    let (harness, _) = open(&storage, &setup).await;
    harness.resume().unwrap();
    let record = settled(&harness, id).await;
    assert_eq!((record.status, record.reason.as_deref()), (SubmissionStatus::Unanswered, Some("interrupted")));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn ends_a_run_someone_aborted_as_aborted_when_its_abort_handler_is_cut_off_restart_limit_times() {
    let storage = Arc::new(ControlledStorage::default());
    let base = deferred_base(3_600_000);
    restart_limit(&base, 2.0);
    base.faux.set_responses([answer("never")]);
    let cancels = Arc::new(AtomicUsize::new(0));
    let counted = cancels.clone();
    // The abort handler of a polling generation cancels its response: make that hang.
    let cancel: CancelFn = Arc::new(move |options| {
        counted.fetch_add(1, Ordering::SeqCst);
        // Pending until the crash aborts this process's handler, which otherwise holds its close open.
        Box::pin(async move {
            if let Some(signal) = options.signal {
                aborted(signal).await;
            }
            Ok(())
        })
    });
    let setup = ChatSetup { models: WrappedModels::cancelling(base.models.clone(), cancel), ..base };
    let (mut harness, root) = open(&storage, &setup).await;
    harness.resume().unwrap();
    let id = root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap().id;
    let task_id = run_task_id(&harness).await;
    let sleeping =
        || async { harness.get_task(task_id, &ctx()).await.unwrap().unwrap().status() == TaskStatus::Sleeping };
    wait_for(sleeping).await;
    let aborting = root.clone();
    tokio::spawn(async move { aborting.abort(&ctx(), false).await });
    for crash in 1..=2 {
        wait_for(|| async { cancels.load(Ordering::SeqCst) == crash }).await;
        kill(&harness).await;
        harness = open(&storage, &setup).await.0;
        assert_eq!(harness.get_task(task_id, &ctx()).await.unwrap().unwrap().restarts, Some(crash as u32));
        harness.resume().unwrap();
    }
    let detail_text = format!("Abort handler of task {task_id} (pi.generation) was cut off 2 times");
    let record = settled(&harness, id).await;
    assert_eq!((record.status, record.reason.as_deref()), (SubmissionStatus::Unanswered, Some("aborted")));
    assert_eq!(detail(&record), Some(json!(detail_text)));
    let failure = json!({ "taskId": task_id, "inputs": [id], "reason": "aborted", "detail": detail_text });
    assert_eq!(live(&harness).await, json!({ "runFailure": failure }));
    let outcome = serde_json::to_value(harness.wait_for_task(task_id, &ctx()).await.unwrap().state.outcome()).unwrap();
    assert_eq!(
        (&outcome["status"], &outcome["error"]["detail"]),
        (&json!("faulted"), &json!({ "reason": "restart_limit" }))
    );
    let faulted = |info: &ReportInfo| matches!(info, ReportInfo::TaskFaulted { reason: FaultReason::RestartLimit, .. });
    wait_for(|| async { setup.infos.lock().unwrap().iter().any(faulted) }).await;
    harness.close(&ctx()).await.unwrap();
}
