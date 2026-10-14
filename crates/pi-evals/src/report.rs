//! Reads per-arm reports, pairs arms, and computes lift, as upstream `report.ts`.
//! Arm reports use the Vitest JSON shape (`testResults[].assertionResults[]` with
//! `meta.eval` and `meta.harness`) that the Rust runner writes.

use std::path::Path;

use serde::Serialize;
use serde_json::{Value, json};

use crate::fsutil::{create_private_dir, ser_opt_f64, sha256_hex, write_private};
use crate::plan::{DocumentationVariant, EvalTask};

pub use crate::report_format::format_eval_comparison_report;
pub use crate::report_summary::{
    BlockedPair, EvalComparisonReport, EvalSetComparison, OperationalMetricTotal, PairedMetricSummary, VariantTotals,
    summarize_eval_observations,
};

pub const PI_SESSION_SNAPSHOT_ARTIFACT: &str = "piSessionJsonl";

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalRunIdentity {
    pub eval_set: String,
    pub case_id: String,
    pub variant: DocumentationVariant,
    pub model: String,
    pub run_number: u32,
}

pub type ExpectedEvalRun = EvalRunIdentity;

impl From<&EvalTask> for EvalRunIdentity {
    fn from(task: &EvalTask) -> Self {
        Self {
            eval_set: task.eval_set.clone(),
            case_id: task.case_id.clone(),
            variant: task.variant,
            model: task.model.clone(),
            run_number: task.run_number,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalMetrics {
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "ser_opt_f64")]
    pub input_tokens: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "ser_opt_f64")]
    pub output_tokens: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "ser_opt_f64")]
    pub cache_read_tokens: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "ser_opt_f64")]
    pub cache_write_tokens: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "ser_opt_f64")]
    pub total_tokens: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "ser_opt_f64")]
    pub tool_calls: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "ser_opt_f64")]
    pub total_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "ser_opt_f64")]
    pub estimated_cost_usd: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Scored,
    Unscored,
    Skipped,
    Pending,
    Errored,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scored => "scored",
            Self::Unscored => "unscored",
            Self::Skipped => "skipped",
            Self::Pending => "pending",
            Self::Errored => "errored",
        }
    }
}

/// One arm's outcome; `score` is set exactly when `outcome` is `Scored`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EvalObservation {
    #[serde(flatten)]
    pub identity: EvalRunIdentity,
    #[serde(flatten)]
    pub metrics: EvalMetrics,
    pub outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "ser_opt_f64")]
    pub score: Option<f64>,
}

impl EvalObservation {
    pub fn new(identity: EvalRunIdentity, metrics: EvalMetrics, outcome: Outcome) -> Self {
        Self { identity, metrics, outcome, score: None }
    }

    pub fn scored(identity: EvalRunIdentity, metrics: EvalMetrics, score: f64) -> Self {
        Self { identity, metrics, outcome: Outcome::Scored, score: Some(score) }
    }
}

fn optional_metric(value: Option<&Value>, name: &str) -> anyhow::Result<Option<f64>> {
    let Some(value) = value else { return Ok(None) };
    match value.as_f64() {
        Some(number) if number.is_finite() && number >= 0.0 => Ok(Some(number)),
        _ => anyhow::bail!("{name} must be a finite non-negative number."),
    }
}

fn validate_score(value: Option<&Value>) -> anyhow::Result<Option<f64>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_f64() {
            Some(score) if score.is_finite() && (0.0..=1.0).contains(&score) => Ok(Some(score)),
            _ => anyhow::bail!("Eval score must be between 0 and 1."),
        },
    }
}

/// Vitest case statuses that end an arm without a score.
pub fn classify_case_status(status: &str) -> Option<Outcome> {
    match status {
        "failed" => Some(Outcome::Errored),
        "skipped" | "todo" | "disabled" => Some(Outcome::Skipped),
        "pending" => Some(Outcome::Pending),
        _ => None,
    }
}

pub fn errored_observation(task: &EvalTask) -> EvalObservation {
    EvalObservation::new(task.into(), EvalMetrics::default(), Outcome::Errored)
}

pub(crate) fn task_identity_hash(task: &EvalTask) -> String {
    let identity = json!([task.eval_set, task.case_id, task.variant, task.model, task.run_number]);
    sha256_hex(identity.to_string().as_bytes())
}

fn persist_session(run: &Value, task: &EvalTask, artifact_directory: &Path) -> anyhow::Result<()> {
    let Some(session) = run["artifacts"][PI_SESSION_SNAPSHOT_ARTIFACT].as_str() else { return Ok(()) };
    let directory = artifact_directory.join(task.variant.as_str()).join("sessions").join(task_identity_hash(task));
    create_private_dir(&directory)?;
    write_private(&directory.join("session.jsonl"), session.as_bytes())?;
    Ok(())
}

fn read_metrics(run: &Value) -> anyhow::Result<EvalMetrics> {
    let usage = &run["usage"];
    let metadata = &usage["metadata"];
    Ok(EvalMetrics {
        input_tokens: optional_metric(usage.get("inputTokens"), "inputTokens")?,
        output_tokens: optional_metric(usage.get("outputTokens"), "outputTokens")?,
        cache_read_tokens: optional_metric(metadata.get("cacheReadTokens"), "cacheReadTokens")?,
        cache_write_tokens: optional_metric(metadata.get("cacheWriteTokens"), "cacheWriteTokens")?,
        total_tokens: optional_metric(usage.get("totalTokens"), "totalTokens")?,
        tool_calls: optional_metric(usage.get("toolCalls"), "toolCalls")?,
        total_ms: optional_metric(run["timings"].get("totalMs"), "totalMs")?,
        estimated_cost_usd: optional_metric(metadata.get("estimatedCostUsd"), "estimatedCostUsd")?,
    })
}

/// Read one arm's report into an observation. Any report that does not describe exactly
/// this task, with a harness run for the task's model, is `errored`.
pub fn read_task_observation(task: &EvalTask, report_path: &Path, artifact_directory: &Path) -> EvalObservation {
    let identity = EvalRunIdentity::from(task);
    let errored = |metrics: EvalMetrics| EvalObservation::new(identity.clone(), metrics, Outcome::Errored);
    let Some(report) = std::fs::read(report_path).ok().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
    else {
        return errored(EvalMetrics::default());
    };
    let Some(results) = report["testResults"].as_array() else { return errored(EvalMetrics::default()) };
    let assertions: Vec<&Value> =
        results.iter().flat_map(|result| result["assertionResults"].as_array().into_iter().flatten()).collect();
    let reported_full_name = format!("{} {}", task.eval_set, task.case_id);
    let [assertion] = assertions.as_slice() else { return errored(EvalMetrics::default()) };
    if assertion["fullName"].as_str() != Some(reported_full_name.as_str()) {
        return errored(EvalMetrics::default());
    }
    let Some(status) = assertion["status"].as_str() else { return errored(EvalMetrics::default()) };
    let status_outcome = classify_case_status(status);
    if let Some(outcome @ (Outcome::Skipped | Outcome::Pending)) = status_outcome {
        return EvalObservation::new(identity, EvalMetrics::default(), outcome);
    }
    let run = &assertion["meta"]["harness"]["run"];
    if !run.is_object() {
        return errored(EvalMetrics::default());
    }
    if persist_session(run, task, artifact_directory).is_err() {
        return errored(EvalMetrics::default());
    }
    let usage = &run["usage"];
    let actual_model = match (usage["provider"].as_str(), usage["model"].as_str()) {
        (Some(provider), Some(model)) if !provider.is_empty() && !model.is_empty() => {
            Some(format!("{provider}/{model}"))
        }
        _ => None,
    };
    if actual_model.as_deref() != Some(task.model.as_str()) {
        return errored(EvalMetrics::default());
    }
    let Ok(metrics) = read_metrics(run) else { return errored(EvalMetrics::default()) };
    let run_errors = run["errors"].as_array().map_or(0, Vec::len);
    if status_outcome == Some(Outcome::Errored) || run_errors > 0 {
        return errored(metrics);
    }
    match validate_score(assertion["meta"]["eval"].get("avgScore")) {
        Err(_) => errored(metrics),
        Ok(None) => EvalObservation::new(identity, metrics, Outcome::Unscored),
        Ok(Some(score)) => EvalObservation::scored(identity, metrics, score),
    }
}
