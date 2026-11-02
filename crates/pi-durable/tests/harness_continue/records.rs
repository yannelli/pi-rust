//! Local regression, not an upstream case: records stored before continues existed still decode, and continue
//! records and the `nothing_to_answer` reason round-trip in upstream's JSON.

use pi_durable::harness::live::RunFailureReason;

use super::*;

fn round_trip(record: Value) -> SubmissionRecord {
    let decoded: SubmissionRecord = serde_json::from_value(record.clone()).unwrap();
    assert_eq!(serde_json::to_value(&decoded).unwrap(), record);
    decoded
}

#[test]
fn decodes_earlier_submission_records_and_round_trips_continue_records() {
    let input = json!({ "conversationId": 1, "type": "input", "status": "done", "entry": 2, "answer": 3, "id": 5 });
    let input = round_trip(input);
    assert_eq!((input.kind, input.status), (SubmissionType::Input, SubmissionStatus::Done));
    let write = round_trip(json!({ "conversationId": 1, "type": "write", "status": "queued", "id": 6 }));
    assert_eq!(write.kind, SubmissionType::Write);
    let continues = [
        json!({ "status": "placed" }),
        json!({ "status": "done", "answer": 8 }),
        json!({ "status": "unanswered", "reason": "nothing_to_answer" }),
    ];
    for mut record in continues {
        let fields = json!({ "conversationId": 1, "type": "continue", "id": 7 });
        record.as_object_mut().unwrap().extend(fields.as_object().unwrap().clone());
        let decoded = round_trip(record);
        assert_eq!((decoded.kind, decoded.entry), (SubmissionType::Continue, None));
    }
    assert_eq!(SubmissionType::Continue.as_str(), "continue");
}

#[test]
fn encodes_nothing_to_answer_beside_earlier_run_failure_reasons() {
    let reasons = [
        (RunFailureReason::ModelError, "model_error"),
        (RunFailureReason::RestartLimit, "restart_limit"),
        (RunFailureReason::NothingToAnswer, "nothing_to_answer"),
    ];
    for (reason, text) in reasons {
        assert_eq!(serde_json::to_value(reason).unwrap(), json!(text));
        assert_eq!(serde_json::from_value::<RunFailureReason>(json!(text)).unwrap(), reason);
        assert_eq!(reason.as_str(), text);
    }
    // A continue carries only a request ID: busy policies and entries do not apply.
    let draft = SubmissionDraft::continue_run().with_request_id("r1").with_when_busy(WhenBusy::Steer);
    assert_eq!(draft, SubmissionDraft::Continue { request_id: Some("r1".into()) });
}
