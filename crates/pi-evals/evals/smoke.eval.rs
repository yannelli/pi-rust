//! Host eval: answer a basic prompt with no tools.

use std::sync::Arc;

use crate::framework::{CaseReport, EvalCase, EvalSuite, Project};
use crate::harness::{HarnessOptions, Step, create_pi_coding_agent_harness, process_env};

async fn returns_the_expected_answer() -> CaseReport {
    let harness = create_pi_coding_agent_harness(HarnessOptions { no_tools: true, ..HarnessOptions::default() });
    let mut report = CaseReport { harness_name: Some(harness.name.clone()), ..CaseReport::default() };
    let prompt = "What's the capital of France? Respond with only the city name.";
    let run = match harness.run(&[Step::Prompt(prompt.into())]).await {
        Ok(run) => run,
        Err(failure) => {
            report.failures.push(failure.error);
            report.run = failure.partial.map(|run| *run);
            return report;
        }
    };
    let env = process_env();
    let output = run.output.as_str().unwrap_or_default().trim().to_string();
    report.check(output == "Paris", format!("expected \"Paris\", got {output:?}"));
    report.check(run.errors.is_empty(), format!("expected no errors, got {:?}", run.errors));
    report.check(run.usage["provider"].as_str() == env.get("PI_PROVIDER").map(String::as_str), "provider mismatch");
    report.check(run.usage["model"].as_str() == env.get("PI_MODEL").map(String::as_str), "model mismatch");
    report.check(run.usage["totalTokens"].as_f64().unwrap_or(0.0) > 0.0, "expected totalTokens > 0");
    report.run = Some(run);
    report
}

pub fn suite() -> EvalSuite {
    EvalSuite {
        file: "evals/smoke.eval.rs",
        project: Project::Host,
        name: "Answer a basic prompt".into(),
        cases: vec![EvalCase {
            name: "returns the expected answer".into(),
            run: Arc::new(|| Box::pin(returns_the_expected_answer())),
        }],
    }
}
