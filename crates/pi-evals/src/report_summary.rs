//! Pairs `without_docs` and `with_docs` arms and summarizes lift and efficiency.

use std::collections::{BTreeMap, HashSet};

use serde::Serialize;

use crate::fsutil::{ser_f64, ser_opt_f64};
use crate::plan::DocumentationVariant;
use crate::report::{EvalObservation, EvalRunIdentity, ExpectedEvalRun, Outcome};

const CONTROL: DocumentationVariant = DocumentationVariant::WithoutDocs;
const TREATMENT: DocumentationVariant = DocumentationVariant::WithDocs;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairedMetricSummary {
    pub eligible_pairs: usize,
    #[serde(serialize_with = "ser_opt_f64")]
    pub control_mean: Option<f64>,
    #[serde(serialize_with = "ser_opt_f64")]
    pub treatment_mean: Option<f64>,
    #[serde(serialize_with = "ser_opt_f64")]
    pub mean_delta: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalSetComparison {
    pub eval_set: String,
    pub total_pairs: usize,
    pub eligible_pairs: usize,
    pub blocked_pairs: usize,
    #[serde(serialize_with = "ser_opt_f64")]
    pub control_pass_rate: Option<f64>,
    #[serde(serialize_with = "ser_opt_f64")]
    pub treatment_pass_rate: Option<f64>,
    #[serde(serialize_with = "ser_opt_f64")]
    pub lift: Option<f64>,
    pub flags: Vec<&'static str>,
    pub total_tokens: PairedMetricSummary,
    pub tool_calls: PairedMetricSummary,
    pub total_ms: PairedMetricSummary,
    pub estimated_cost_usd: PairedMetricSummary,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockedPair {
    pub eval_set: String,
    pub case_id: String,
    pub model: String,
    pub run_number: u32,
    pub reasons: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationalMetricTotal {
    pub available_runs: usize,
    #[serde(serialize_with = "ser_opt_f64")]
    pub total: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VariantTotals {
    pub variant: DocumentationVariant,
    pub runs: usize,
    pub input_tokens: OperationalMetricTotal,
    pub output_tokens: OperationalMetricTotal,
    pub cache_read_tokens: OperationalMetricTotal,
    pub cache_write_tokens: OperationalMetricTotal,
    pub total_tokens: OperationalMetricTotal,
    pub tool_calls: OperationalMetricTotal,
    pub total_ms: OperationalMetricTotal,
    pub estimated_cost_usd: OperationalMetricTotal,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalComparisonReport {
    #[serde(serialize_with = "ser_f64")]
    pub schema_version: f64,
    pub protocol_digest: String,
    pub control: DocumentationVariant,
    pub treatment: DocumentationVariant,
    pub comparisons: Vec<EvalSetComparison>,
    pub blocked_pairs: Vec<BlockedPair>,
    pub operational_totals: Vec<VariantTotals>,
}

struct Pair<'a> {
    control: &'a EvalObservation,
    treatment: &'a EvalObservation,
}

#[derive(Default)]
struct PairGroup<'a> {
    expected: BTreeMap<DocumentationVariant, usize>,
    observations: BTreeMap<DocumentationVariant, Vec<&'a EvalObservation>>,
}

type PairKey = (String, String, String, u32);

fn pair_key(identity: &EvalRunIdentity) -> PairKey {
    (identity.eval_set.clone(), identity.case_id.clone(), identity.model.clone(), identity.run_number)
}

fn mean(values: &[f64]) -> Option<f64> {
    (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
}

/// `Number((treatment - control).toPrecision(15))`.
fn difference(treatment: f64, control: f64) -> f64 {
    format!("{:.14e}", treatment - control).parse().unwrap_or(treatment - control)
}

fn group_pairs<'a>(expected: &[ExpectedEvalRun], observations: &'a [EvalObservation]) -> Vec<(PairKey, PairGroup<'a>)> {
    let mut groups: BTreeMap<PairKey, PairGroup<'a>> = BTreeMap::new();
    for run in expected {
        *groups.entry(pair_key(run)).or_default().expected.entry(run.variant).or_default() += 1;
    }
    for observation in observations {
        let group = groups.entry(pair_key(&observation.identity)).or_default();
        group.observations.entry(observation.identity.variant).or_default().push(observation);
    }
    groups.into_iter().collect()
}

fn resolve_pair<'a>(key: &PairKey, group: &PairGroup<'a>) -> Result<Pair<'a>, BlockedPair> {
    let mut reasons = Vec::new();
    for variant in [CONTROL, TREATMENT] {
        let expected = group.expected.get(&variant).copied().unwrap_or(0);
        let observed = group.observations.get(&variant).map_or(&[][..], Vec::as_slice);
        if expected != 1 {
            reasons.push(format!("{variant}: design expected 1 run, found {expected}"));
        }
        if observed.len() != expected {
            let plural = if expected == 1 { "" } else { "s" };
            reasons.push(format!("{variant}: expected {expected} observation{plural}, found {}", observed.len()));
        }
        if expected == 1 && observed.len() == 1 && observed[0].outcome != Outcome::Scored {
            reasons.push(format!("{variant}: {}", observed[0].outcome.as_str()));
        }
    }
    if !reasons.is_empty() {
        let (eval_set, case_id, model, run_number) = key.clone();
        return Err(BlockedPair { eval_set, case_id, model, run_number, reasons });
    }
    Ok(Pair { control: group.observations[&CONTROL][0], treatment: group.observations[&TREATMENT][0] })
}

type Select = fn(&EvalObservation) -> Option<f64>;

fn summarize_metric(pairs: &[Pair<'_>], select: Select) -> PairedMetricSummary {
    let (mut control, mut treatment) = (Vec::new(), Vec::new());
    for pair in pairs {
        if let (Some(c), Some(t)) = (select(pair.control), select(pair.treatment)) {
            control.push(c);
            treatment.push(t);
        }
    }
    let (control_mean, treatment_mean) = (mean(&control), mean(&treatment));
    PairedMetricSummary {
        eligible_pairs: control.len(),
        control_mean,
        treatment_mean,
        mean_delta: control_mean.zip(treatment_mean).map(|(c, t)| difference(t, c)),
    }
}

fn operational_total(runs: &[&EvalObservation], select: Select) -> OperationalMetricTotal {
    let values: Vec<f64> = runs.iter().filter_map(|run| select(run)).collect();
    OperationalMetricTotal { available_runs: values.len(), total: (!values.is_empty()).then(|| values.iter().sum()) }
}

fn variant_totals(observations: &[EvalObservation], variant: DocumentationVariant) -> VariantTotals {
    let runs: Vec<&EvalObservation> = observations.iter().filter(|run| run.identity.variant == variant).collect();
    VariantTotals {
        variant,
        runs: runs.len(),
        input_tokens: operational_total(&runs, |run| run.metrics.input_tokens),
        output_tokens: operational_total(&runs, |run| run.metrics.output_tokens),
        cache_read_tokens: operational_total(&runs, |run| run.metrics.cache_read_tokens),
        cache_write_tokens: operational_total(&runs, |run| run.metrics.cache_write_tokens),
        total_tokens: operational_total(&runs, |run| run.metrics.total_tokens),
        tool_calls: operational_total(&runs, |run| run.metrics.tool_calls),
        total_ms: operational_total(&runs, |run| run.metrics.total_ms),
        estimated_cost_usd: operational_total(&runs, |run| run.metrics.estimated_cost_usd),
    }
}

fn comparison_flags(pairs: &[Pair<'_>], control: Option<f64>, treatment: Option<f64>) -> Vec<&'static str> {
    let mut flags = Vec::new();
    if let (Some(control), Some(treatment)) = (control, treatment) {
        if control == treatment {
            flags.push("no-lift");
        }
        if treatment < control {
            flags.push("negative-delta");
        }
        if control == 1.0 {
            flags.push("control-saturated");
        }
        if treatment == 1.0 {
            flags.push("treatment-saturated");
        }
    }
    let mut outcomes: BTreeMap<(String, DocumentationVariant), HashSet<bool>> = BTreeMap::new();
    for pair in pairs {
        for run in [pair.control, pair.treatment] {
            let passed = run.score.unwrap_or(0.0) >= 1.0;
            outcomes.entry((run.identity.case_id.clone(), run.identity.variant)).or_default().insert(passed);
        }
    }
    if outcomes.values().any(|values| values.len() > 1) {
        flags.push("flaky");
    }
    flags
}

fn pass_rate<'a>(pairs: &[Pair<'a>], pick: fn(&Pair<'a>) -> &'a EvalObservation) -> f64 {
    pairs.iter().filter(|pair| pick(pair).score.unwrap_or(0.0) >= 1.0).count() as f64 / pairs.len() as f64
}

/// Pair expected arms with observations; any blocked pair in an eval set withholds its
/// headline pass rates.
pub fn summarize_eval_observations(
    protocol_digest: &str,
    expected_runs: &[ExpectedEvalRun],
    observations: &[EvalObservation],
) -> EvalComparisonReport {
    let mut blocked_pairs = Vec::new();
    let mut by_eval_set: BTreeMap<String, (usize, Vec<Pair<'_>>)> = BTreeMap::new();
    for (key, group) in group_pairs(expected_runs, observations) {
        let entry = by_eval_set.entry(key.0.clone()).or_default();
        entry.0 += 1;
        match resolve_pair(&key, &group) {
            Ok(pair) => entry.1.push(pair),
            Err(blocked) => blocked_pairs.push(blocked),
        }
    }
    let comparisons = by_eval_set
        .into_iter()
        .map(|(eval_set, (total_pairs, pairs))| {
            let blocked = total_pairs - pairs.len();
            let headline = blocked == 0 && !pairs.is_empty();
            let control_pass_rate = headline.then(|| pass_rate(&pairs, |pair| pair.control));
            let treatment_pass_rate = headline.then(|| pass_rate(&pairs, |pair| pair.treatment));
            EvalSetComparison {
                eval_set,
                total_pairs,
                eligible_pairs: pairs.len(),
                blocked_pairs: blocked,
                control_pass_rate,
                treatment_pass_rate,
                lift: control_pass_rate.zip(treatment_pass_rate).map(|(c, t)| difference(t, c)),
                flags: comparison_flags(&pairs, control_pass_rate, treatment_pass_rate),
                total_tokens: summarize_metric(&pairs, |run| run.metrics.total_tokens),
                tool_calls: summarize_metric(&pairs, |run| run.metrics.tool_calls),
                total_ms: summarize_metric(&pairs, |run| run.metrics.total_ms),
                estimated_cost_usd: summarize_metric(&pairs, |run| run.metrics.estimated_cost_usd),
            }
        })
        .collect();
    EvalComparisonReport {
        schema_version: 3.0,
        protocol_digest: protocol_digest.to_string(),
        control: CONTROL,
        treatment: TREATMENT,
        comparisons,
        blocked_pairs,
        operational_totals: vec![variant_totals(observations, CONTROL), variant_totals(observations, TREATMENT)],
    }
}
