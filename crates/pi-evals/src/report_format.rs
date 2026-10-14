//! Text rendering of a comparison report.

use crate::report::{EvalComparisonReport, OperationalMetricTotal, PairedMetricSummary};

fn bold(text: &str) -> String {
    format!("\x1b[1m{text}\x1b[22m")
}

fn percentage(value: Option<f64>) -> String {
    value.map_or_else(|| "unavailable".to_string(), |value| format!("{:.1}%", value * 100.0))
}

fn signed(value: f64, digits: usize) -> String {
    format!("{}{value:.digits$}", if value >= 0.0 { "+" } else { "" })
}

fn paired_metric(label: &str, metric: &PairedMetricSummary, unit: &str) -> String {
    match (metric.mean_delta, metric.control_mean, metric.treatment_mean) {
        (Some(delta), Some(control), Some(treatment)) => format!(
            "    {label:>10}  {}{unit} (with {treatment:.1}{unit}, without {control:.1}{unit}, {} pairs)",
            signed(delta, 1),
            metric.eligible_pairs
        ),
        _ => format!("    {label:>10}  unavailable"),
    }
}

fn operational_metric(metric: &OperationalMetricTotal, runs: usize, format: impl Fn(f64) -> String) -> String {
    let Some(total) = metric.total else { return format!("unavailable (0/{runs} measured)") };
    let coverage = if metric.available_runs == runs {
        String::new()
    } else {
        format!(" ({}/{runs} measured)", metric.available_runs)
    };
    format!("{}{coverage}", format(total))
}

/// The human-readable report; empty when there are no comparisons.
pub fn format_eval_comparison_report(report: &EvalComparisonReport) -> String {
    if report.comparisons.is_empty() {
        return String::new();
    }
    let mut lines = vec![bold("Documentation Eval Comparisons")];
    for comparison in &report.comparisons {
        lines.push(format!("  {}", comparison.eval_set));
        lines.push(format!("         Pairs  {}/{} eligible", comparison.eligible_pairs, comparison.total_pairs));
        match comparison.lift {
            None if comparison.blocked_pairs > 0 => {
                lines.push("     Pass rate  withheld because pairs are blocked".into());
            }
            None => lines.push("     Pass rate  unavailable".into()),
            Some(lift) => lines.push(format!(
                "     Pass rate  {} pp (with {}, without {})",
                signed(lift * 100.0, 1),
                percentage(comparison.treatment_pass_rate),
                percentage(comparison.control_pass_rate)
            )),
        }
        if !comparison.flags.is_empty() {
            lines.push(format!("         Flags  {}", comparison.flags.join(", ")));
        }
        lines.push(paired_metric("Tokens", &comparison.total_tokens, ""));
        lines.push(paired_metric("Tools", &comparison.tool_calls, ""));
        lines.push(paired_metric("Latency", &comparison.total_ms, "ms"));
        let cost = &comparison.estimated_cost_usd;
        match (cost.mean_delta, cost.control_mean, cost.treatment_mean) {
            (Some(delta), Some(control), Some(treatment)) => lines.push(format!(
                "     Est. cost  {}${:.4} (with ${treatment:.4}, without ${control:.4}, {} pairs)",
                if delta >= 0.0 { "+" } else { "-" },
                delta.abs(),
                cost.eligible_pairs
            )),
            _ => lines.push("     Est. cost  unavailable".into()),
        }
    }
    lines.push("  Operational totals".into());
    for totals in &report.operational_totals {
        let runs = totals.runs;
        let tokens = operational_metric(&totals.total_tokens, runs, |total| format!("{total} tokens"));
        let tools = operational_metric(&totals.tool_calls, runs, |total| format!("{total} tools"));
        let latency = operational_metric(&totals.total_ms, runs, |total| format!("{:.2}s", total / 1000.0));
        let cost = operational_metric(&totals.estimated_cost_usd, runs, |total| format!("${total:.4} cost"));
        lines.push(format!("    {}: {runs} runs, {tokens}, {tools}, {latency}, {cost}", totals.variant));
    }
    if !report.blocked_pairs.is_empty() {
        lines.push("  Blocked pairs".into());
        for blocked in &report.blocked_pairs {
            lines.push(format!(
                "    {}/{}/{}/run-{}: {}",
                blocked.eval_set,
                blocked.case_id,
                blocked.model,
                blocked.run_number,
                blocked.reasons.join("; ")
            ));
        }
    }
    lines.join("\n")
}
