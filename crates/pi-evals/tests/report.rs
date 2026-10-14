//! Port of upstream `test/report.test.ts`.

use std::path::Path;

use pi_evals::plan::{DocumentationVariant, EvalTask};
use pi_evals::report::{
    EvalMetrics, EvalObservation, EvalRunIdentity, Outcome, PI_SESSION_SNAPSHOT_ARTIFACT, classify_case_status,
    read_task_observation,
};
use serde_json::{Map, Value, json};
use tempfile::TempDir;

const SESSION: &str = "{\"type\":\"session\"}\n";

fn task() -> EvalTask {
    EvalTask {
        file: "evals/example.docs.eval.rs".into(),
        full_name: "Example workflow > handles the case".into(),
        eval_set: "Example workflow".into(),
        case_id: "handles the case".into(),
        variant: DocumentationVariant::WithoutDocs,
        model: "fixture/model".into(),
        run_number: 1,
    }
}

fn write_task_report(status: &str, meta: Value) -> (TempDir, std::path::PathBuf) {
    let directory = tempfile::Builder::new().prefix("pi-eval-report-test-").tempdir().unwrap();
    let report_path = directory.path().join("vitest.json");
    let task = task();
    let pending = u32::from(status == "pending" || status == "skipped");
    let report = json!({
        "numFailedTests": 0, "numPassedTests": u32::from(status == "passed"), "numPendingTests": pending,
        "numTodoTests": 0, "numTotalTests": 1, "startTime": 0, "success": true,
        "testResults": [{
            "message": "", "name": "/repo/crates/pi-evals/evals/example.docs.eval.rs", "status": "passed",
            "assertionResults": [{
                "ancestorTitles": [task.eval_set], "fullName": format!("{} {}", task.eval_set, task.case_id),
                "status": status, "title": task.case_id, "failureMessages": [], "meta": meta,
            }],
        }],
    });
    std::fs::write(&report_path, report.to_string()).unwrap();
    (directory, report_path)
}

#[derive(Default)]
struct MetaOverrides {
    avg_score: Option<Value>,
    model: Option<&'static str>,
    errors: Option<Value>,
    artifacts: Option<Value>,
}

fn scored_meta(overrides: MetaOverrides) -> Value {
    let artifacts = overrides.artifacts.unwrap_or_else(|| {
        let mut artifacts = Map::new();
        artifacts.insert("runId".into(), json!("run-1"));
        artifacts.insert(PI_SESSION_SNAPSHOT_ARTIFACT.into(), json!(SESSION));
        Value::Object(artifacts)
    });
    json!({
        "eval": {
            "avgScore": overrides.avg_score.unwrap_or(json!(0.5)),
            "scores": [{ "name": "StructuredOutputJudge", "score": 0.5 }],
            "thresholdFailed": false,
        },
        "harness": {
            "name": "without_docs",
            "run": {
                "output": { "ok": true },
                "session": { "events": [{ "type": "message", "role": "user", "content": "prompt" }] },
                "usage": {
                    "provider": "fixture", "model": overrides.model.unwrap_or("model"),
                    "inputTokens": 10, "outputTokens": 5, "totalTokens": 15, "toolCalls": 1,
                    "metadata": { "cacheReadTokens": 2, "cacheWriteTokens": 3, "estimatedCostUsd": 0.01 },
                },
                "timings": { "totalMs": 1234 },
                "artifacts": artifacts,
                "errors": overrides.errors.unwrap_or(json!([])),
            },
        },
    })
}

fn read_observation(status: &str, meta: Value) -> (TempDir, EvalObservation) {
    let (directory, report_path) = write_task_report(status, meta);
    let observation = read_task_observation(&task(), &report_path, directory.path());
    (directory, observation)
}

fn full_metrics() -> EvalMetrics {
    EvalMetrics {
        input_tokens: Some(10.0),
        output_tokens: Some(5.0),
        cache_read_tokens: Some(2.0),
        cache_write_tokens: Some(3.0),
        total_tokens: Some(15.0),
        tool_calls: Some(1.0),
        total_ms: Some(1234.0),
        estimated_cost_usd: Some(0.01),
    }
}

fn persisted_sessions(directory: &Path) -> Vec<String> {
    let sessions = directory.join("without_docs").join("sessions");
    let hashes: Vec<_> = std::fs::read_dir(&sessions).unwrap().map(|entry| entry.unwrap().path()).collect();
    hashes.iter().map(|hash| std::fs::read_to_string(hash.join("session.jsonl")).unwrap()).collect()
}

#[test]
fn maps_skipped_todo_and_disabled_to_skipped() {
    for status in ["skipped", "todo", "disabled"] {
        assert_eq!(classify_case_status(status), Some(Outcome::Skipped), "{status}");
    }
}

#[test]
fn maps_failed_infrastructure_to_errored() {
    assert_eq!(classify_case_status("failed"), Some(Outcome::Errored));
}

#[test]
fn preserves_a_skipped_outcome_when_no_harness_run_exists() {
    assert_eq!(read_observation("skipped", json!({})).1.outcome, Outcome::Skipped);
}

#[test]
fn preserves_a_pending_outcome_when_no_harness_run_exists() {
    assert_eq!(read_observation("pending", json!({})).1.outcome, Outcome::Pending);
}

#[test]
fn preserves_metrics_from_a_failed_eval_with_a_partial_harness_run() {
    let errors = json!([{ "message": "Prompt verification failed" }]);
    let (directory, observation) =
        read_observation("failed", scored_meta(MetaOverrides { errors: Some(errors), ..MetaOverrides::default() }));
    let identity = EvalRunIdentity::from(&task());
    assert_eq!(observation, EvalObservation::new(identity, full_metrics(), Outcome::Errored));
    assert_eq!(persisted_sessions(directory.path()), vec![SESSION.to_string()]);
}

#[test]
fn records_an_errored_outcome_when_a_passed_eval_has_no_harness_run() {
    assert_eq!(read_observation("passed", json!({})).1.outcome, Outcome::Errored);
}

#[test]
fn reads_a_scored_harness_run_and_persists_the_session_artifact() {
    let (directory, observation) = read_observation("passed", scored_meta(MetaOverrides::default()));
    let identity = EvalRunIdentity::from(&task());
    assert_eq!(observation, EvalObservation::scored(identity, full_metrics(), 0.5));
    assert_eq!(persisted_sessions(directory.path()), vec![SESSION.to_string()]);
}

#[test]
fn treats_a_zero_score_as_scored_data() {
    let meta = scored_meta(MetaOverrides { avg_score: Some(json!(0)), ..MetaOverrides::default() });
    let observation = read_observation("passed", meta).1;
    assert_eq!((observation.outcome, observation.score), (Outcome::Scored, Some(0.0)));
}

#[test]
fn records_an_unscored_outcome_when_a_completed_eval_has_no_score() {
    let meta = scored_meta(MetaOverrides { avg_score: Some(Value::Null), ..MetaOverrides::default() });
    assert_eq!(read_observation("passed", meta).1.outcome, Outcome::Unscored);
}

#[test]
fn records_an_errored_outcome_when_the_reported_model_does_not_match_the_task() {
    let meta = scored_meta(MetaOverrides { model: Some("other"), ..MetaOverrides::default() });
    assert_eq!(read_observation("passed", meta).1.outcome, Outcome::Errored);
}

#[test]
fn records_an_errored_outcome_when_a_completed_harness_run_contains_errors() {
    let meta = scored_meta(MetaOverrides { errors: Some(json!([{ "message": "boom" }])), ..MetaOverrides::default() });
    assert_eq!(read_observation("passed", meta).1.outcome, Outcome::Errored);
}

#[test]
fn still_scores_a_completed_eval_when_the_session_artifact_is_missing() {
    let meta = scored_meta(MetaOverrides { artifacts: Some(json!({ "runId": "run-1" })), ..MetaOverrides::default() });
    let observation = read_observation("passed", meta).1;
    assert_eq!((observation.outcome, observation.score), (Outcome::Scored, Some(0.5)));
}
