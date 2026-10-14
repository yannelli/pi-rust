//! Port of upstream `test/comparison.test.ts`.

use pi_evals::plan::DocumentationVariant::{self, WithDocs, WithoutDocs};
use pi_evals::report::{
    EvalMetrics, EvalObservation, EvalRunIdentity, ExpectedEvalRun, OperationalMetricTotal, Outcome,
    PairedMetricSummary, format_eval_comparison_report, summarize_eval_observations,
};

fn identity(variant: DocumentationVariant, run_number: u32) -> EvalRunIdentity {
    EvalRunIdentity {
        eval_set: "tool access".into(),
        case_id: "create".into(),
        variant,
        model: "fixture/model".into(),
        run_number,
    }
}

#[derive(Default)]
struct Overrides {
    total_tokens: Option<f64>,
    tool_calls: Option<f64>,
    total_ms: Option<f64>,
}

fn scored(variant: DocumentationVariant, run_number: u32, score: f64, overrides: Overrides) -> EvalObservation {
    let metrics = EvalMetrics {
        total_tokens: Some(overrides.total_tokens.unwrap_or(100.0)),
        tool_calls: Some(overrides.tool_calls.unwrap_or(2.0)),
        total_ms: Some(overrides.total_ms.unwrap_or(1000.0)),
        estimated_cost_usd: Some(0.01),
        ..EvalMetrics::default()
    };
    EvalObservation::scored(identity(variant, run_number), metrics, score)
}

fn errored(variant: DocumentationVariant, run_number: u32, total_tokens: Option<f64>) -> EvalObservation {
    let metrics = EvalMetrics { total_tokens, ..EvalMetrics::default() };
    EvalObservation::new(identity(variant, run_number), metrics, Outcome::Errored)
}

fn expected_for(run_numbers: &[u32]) -> Vec<ExpectedEvalRun> {
    run_numbers.iter().flat_map(|&run| [identity(WithoutDocs, run), identity(WithDocs, run)]).collect()
}

fn tokens(total_tokens: f64) -> Overrides {
    Overrides { total_tokens: Some(total_tokens), ..Overrides::default() }
}

#[test]
fn computes_paired_lift_and_efficiency_deltas() {
    let observations = vec![
        scored(
            WithoutDocs,
            1,
            0.0,
            Overrides { total_tokens: Some(100.0), tool_calls: Some(3.0), total_ms: Some(1000.0) },
        ),
        scored(WithDocs, 1, 1.0, Overrides { total_tokens: Some(120.0), tool_calls: Some(2.0), total_ms: Some(800.0) }),
        scored(WithoutDocs, 2, 1.0, tokens(200.0)),
        scored(WithDocs, 2, 1.0, tokens(180.0)),
    ];
    let report = summarize_eval_observations("digest", &expected_for(&[1, 2]), &observations);
    assert_eq!(report.comparisons.len(), 1);
    let comparison = &report.comparisons[0];
    assert_eq!(comparison.eval_set, "tool access");
    assert_eq!((comparison.total_pairs, comparison.eligible_pairs, comparison.blocked_pairs), (2, 2, 0));
    assert_eq!(comparison.control_pass_rate, Some(0.5));
    assert_eq!(comparison.treatment_pass_rate, Some(1.0));
    assert_eq!(comparison.lift, Some(0.5));
    let summary = |c: f64, t: f64, d: f64| PairedMetricSummary {
        eligible_pairs: 2,
        control_mean: Some(c),
        treatment_mean: Some(t),
        mean_delta: Some(d),
    };
    assert_eq!(comparison.total_tokens, summary(150.0, 150.0, 0.0));
    assert_eq!(comparison.tool_calls, summary(2.5, 2.0, -0.5));
}

#[test]
fn fails_closed_for_incomplete_and_errored_pairs_while_retaining_totals() {
    let observations = vec![
        scored(WithoutDocs, 1, 0.0, Overrides::default()),
        errored(WithDocs, 1, Some(120.0)),
        scored(WithoutDocs, 2, 1.0, tokens(200.0)),
    ];
    let report = summarize_eval_observations("digest", &expected_for(&[1, 2]), &observations);
    let comparison = &report.comparisons[0];
    assert_eq!((comparison.total_pairs, comparison.eligible_pairs, comparison.blocked_pairs), (2, 0, 2));
    assert_eq!(comparison.lift, None);
    assert_eq!(report.blocked_pairs.len(), 2);
    assert_eq!(report.blocked_pairs[0].run_number, 1);
    assert_eq!(report.blocked_pairs[0].reasons, vec!["with_docs: errored"]);
    assert_eq!(report.blocked_pairs[1].run_number, 2);
    assert_eq!(report.blocked_pairs[1].reasons, vec!["with_docs: expected 1 observation, found 0"]);
    assert_eq!(
        report.operational_totals[0].total_tokens,
        OperationalMetricTotal { available_runs: 2, total: Some(300.0) }
    );
}

#[test]
fn blocks_duplicate_observations_and_keeps_missing_metrics_distinct_from_zero() {
    let mut without_tokens = scored(WithoutDocs, 1, 1.0, Overrides::default());
    without_tokens.metrics.total_tokens = None;
    let observations = vec![without_tokens.clone(), without_tokens, scored(WithDocs, 1, 1.0, tokens(0.0))];
    let report = summarize_eval_observations("digest", &expected_for(&[1]), &observations);
    assert_eq!(report.blocked_pairs[0].reasons, vec!["without_docs: expected 1 observation, found 2"]);
    assert_eq!(report.operational_totals[0].total_tokens, OperationalMetricTotal { available_runs: 0, total: None });
    assert_eq!(
        report.operational_totals[1].total_tokens,
        OperationalMetricTotal { available_runs: 1, total: Some(0.0) }
    );
}

fn strip_ansi(text: &str) -> String {
    regex::Regex::new("\x1b\\[[0-9;]*m").unwrap().replace_all(text, "").into_owned()
}

#[test]
fn formats_blocked_comparisons_and_operational_totals() {
    let observations =
        vec![scored(WithoutDocs, 1, 1.0, Overrides::default()), scored(WithDocs, 1, 1.0, Overrides::default())];
    let report = summarize_eval_observations("digest", &expected_for(&[1, 2]), &observations);
    let formatted = strip_ansi(&format_eval_comparison_report(&report));
    assert!(formatted.contains("Documentation Eval Comparisons"));
    assert!(formatted.contains("Pass rate  withheld because pairs are blocked"));
    assert!(formatted.contains("without_docs: 1 runs"));
}
