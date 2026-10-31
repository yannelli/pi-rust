//! Upstream "crash edges" at the restart limit and "reports of work crashes ended"; the normal-path case is
//! `tests/harness_restart_settings.rs`.

use super::*;

#[tokio::test]
async fn charges_a_request_once_when_a_crash_is_followed_by_a_clean_close_before_the_charge() {
    let storage = Arc::new(ControlledStorage::default());
    let first = streaming();
    retry(&first, true, 3, 1);
    let (id, task_id) = crash_while_streaming(&storage, &first).await;
    let setup = chat_setup(Default::default());
    retry(&setup, true, 3, 1);
    setup.faux.set_responses([answer("answer")]);
    let (harness, _) = open(&storage, &setup).await;
    harness.close(&ctx()).await.unwrap();
    let (harness, root) = open(&storage, &setup).await;
    assert_eq!(harness.get_task(task_id, &ctx()).await.unwrap().unwrap().restarts, Some(1));
    let values = live_values(&harness);
    harness.resume().unwrap();
    assert_eq!(settled(&harness, id).await.status, SubmissionStatus::Done);
    let values = values.get();
    let charged: Vec<Value> = values
        .iter()
        .filter(|value| value["generation"].get("retry").is_some())
        .map(|value| value["generation"]["attempt"].clone())
        .collect();
    assert_eq!(charged, [json!(1)]);
    assert_eq!(kinds(&all_entries(&root).await), ["pi.user", "pi.assistant", "pi.assistant"]);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn ends_a_run_restart_limit_when_charging_the_interrupted_request_keeps_being_cut_off() {
    let storage = Arc::new(ControlledStorage::default());
    let first = streaming();
    restart_limit(&first, 1.0);
    let (id, task_id) = crash_while_streaming(&storage, &first).await;
    // At the limit: the partial is still kept.
    let setup = chat_setup(Default::default());
    restart_limit(&setup, 1.0);
    let (harness, root) = open(&storage, &setup).await;
    let partial = partial_text(&live(&harness).await).unwrap();
    harness.resume().unwrap();
    let text = "The generation was interrupted once in its request phase";
    let record = settled(&harness, id).await;
    assert_eq!((record.status, record.reason.as_deref()), (SubmissionStatus::Unanswered, Some("restart_limit")));
    assert_eq!(detail(&record), Some(json!(text)));
    let failure = json!({ "taskId": task_id, "inputs": [id], "reason": "restart_limit", "detail": text });
    assert_eq!(live(&harness).await, json!({ "runFailure": failure }));
    let entries = all_entries(&root).await;
    assert_eq!(kinds(&entries), ["pi.user", "pi.assistant"]);
    assert_eq!(text_of(entries[1].model.as_ref().and_then(|model| model.first())), Some(partial));
    assert_eq!(setup.faux.state().call_count, 0);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn drops_the_partial_past_the_limit_where_converting_it_may_be_what_keeps_failing() {
    let storage = Arc::new(ControlledStorage::default());
    let (id, task_id) = crash_while_streaming(&storage, &streaming()).await;
    // As if the charge had been cut off twice before: the open after this crash counts 3, past the limit of 2.
    storage.reopen();
    let record = storage.task(task_id, &ctx()).await.unwrap().unwrap();
    let write = StorageWrite::Task { value: TaskRecord { restarts: Some(2), ..record } };
    storage.commit(&[write], &|_| {}, &ctx()).await.unwrap();
    let setup = chat_setup(Default::default());
    restart_limit(&setup, 2.0);
    let (harness, root) = open(&storage, &setup).await;
    harness.resume().unwrap();
    let record = settled(&harness, id).await;
    assert_eq!((record.status, record.reason.as_deref()), (SubmissionStatus::Unanswered, Some("restart_limit")));
    let text = "The generation was interrupted 3 times in a row in its request phase";
    assert_eq!(detail(&record), Some(json!(text)));
    assert_eq!(kinds(&all_entries(&root).await), ["pi.user"]);
    assert!(live(&harness).await.get("generation").is_none());
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn reports_task_interrupted_for_a_run_that_ends_interrupted_and_for_a_task_that_ends_at_the_limit() {
    let storage = Arc::new(ControlledStorage::default());
    let first = streaming();
    retry(&first, false, 0, 0);
    let (_, task_id) = crash_while_streaming(&storage, &first).await;
    let setup = chat_setup(Default::default());
    retry(&setup, false, 0, 0);
    let (harness, _) = open(&storage, &setup).await;
    harness.resume().unwrap();
    harness.wait_for_task(task_id, &ctx()).await.unwrap();
    wait_for(|| async { !setup.infos.lock().unwrap().is_empty() }).await;
    let infos = serde_json::to_value(&*setup.infos.lock().unwrap()).unwrap();
    let task = json!({ "id": task_id, "kind": "pi.generation", "conversationId": ROOT_CONVERSATION_ID });
    assert_eq!(infos, json!([{ "kind": "task_interrupted", "task": task, "reason": "interrupted", "restarts": 1 }]));
    assert!(setup.reports()[0].contains(&format!("{STOPPED_ANSWERING} 1")), "{:?}", setup.reports());
    harness.close(&ctx()).await.unwrap();
}
