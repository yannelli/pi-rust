//! A small stand-in for vitest-evals: suites of named cases, judges that score harness
//! output, and a runner that writes the Vitest JSON report shape `report.rs` reads.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::future::BoxFuture;
use regex::Regex;
use serde_json::{Value, json};

use crate::harness::{Harness, HarnessRun, Step};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Project {
    /// `*.eval.rs` suites that run on this machine.
    Host,
    /// `*.docs.eval.rs` suites that run inside a documentation-variant container.
    Docs,
}

impl Project {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "host" => Some(Self::Host),
            "docs" => Some(Self::Docs),
            _ => None,
        }
    }
}

/// One case's result: Vitest status, failure messages, and the harness and eval metadata.
#[derive(Clone, Debug, Default)]
pub struct CaseReport {
    pub failures: Vec<String>,
    pub harness_name: Option<String>,
    pub run: Option<HarnessRun>,
    pub eval: Option<Value>,
}

impl CaseReport {
    pub fn passed(&self) -> bool {
        self.failures.is_empty()
    }

    /// Record an assertion failure unless `condition` holds.
    pub fn check(&mut self, condition: bool, message: impl Into<String>) {
        if !condition {
            self.failures.push(message.into());
        }
    }
}

pub type CaseFn = Arc<dyn Fn() -> BoxFuture<'static, CaseReport> + Send + Sync>;

#[derive(Clone)]
pub struct EvalCase {
    pub name: String,
    pub run: CaseFn,
}

#[derive(Clone)]
pub struct EvalSuite {
    /// The suite's source path relative to the crate root, as discovery reports it.
    pub file: &'static str,
    pub project: Project,
    pub name: String,
    pub cases: Vec<EvalCase>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct JudgeScore {
    pub name: String,
    pub score: Option<f64>,
    pub metadata: Value,
}

pub trait Judge: Send + Sync {
    fn judge(&self, input: &[Step], run: &HarnessRun) -> JudgeScore;
}

/// Exact structured output: score 1 when the output equals `expected`, extra fields included.
pub struct StructuredOutputJudge {
    pub expected: Value,
}

impl Judge for StructuredOutputJudge {
    fn judge(&self, _input: &[Step], run: &HarnessRun) -> JudgeScore {
        let matched = crate::fsutil::json_equal(&run.output, &self.expected);
        JudgeScore {
            name: "StructuredOutputJudge".into(),
            score: Some(if matched { 1.0 } else { 0.0 }),
            metadata: json!({ "expected": self.expected, "actual": run.output }),
        }
    }
}

/// The share of expected tools called successfully with exactly the expected arguments.
pub struct ToolCallJudge {
    pub expected_tools: Vec<(String, Value)>,
}

impl Judge for ToolCallJudge {
    fn judge(&self, _input: &[Step], run: &HarnessRun) -> JudgeScore {
        let calls = run.tool_calls();
        let found = self.expected_tools.iter().filter(|(name, arguments)| {
            calls
                .iter()
                .any(|call| call.ok && &call.name == name && crate::fsutil::json_equal(&call.arguments, arguments))
        });
        let score =
            if self.expected_tools.is_empty() { 1.0 } else { found.count() as f64 / self.expected_tools.len() as f64 };
        JudgeScore { name: "ToolCallJudge".into(), score: Some(score), metadata: json!({}) }
    }
}

/// Run `harness` on `input`, then score with every judge. A judge threshold of `None`
/// makes low scores data rather than failures.
pub async fn evaluate(
    harness: anyhow::Result<Harness>,
    judges: &[Arc<dyn Judge>],
    threshold: Option<f64>,
    input: &[Step],
) -> CaseReport {
    let mut report = CaseReport::default();
    let harness = match harness {
        Ok(harness) => harness,
        Err(error) => {
            report.failures.push(format!("{error:#}"));
            return report;
        }
    };
    report.harness_name = Some(harness.name.clone());
    match harness.run(input).await {
        Err(failure) => {
            report.failures.push(failure.error);
            report.run = failure.partial.map(|run| *run);
        }
        Ok(run) => {
            let scores: Vec<JudgeScore> = judges.iter().map(|judge| judge.judge(input, &run)).collect();
            let values: Vec<f64> = scores.iter().filter_map(|score| score.score).collect();
            let average = (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64);
            let threshold_failed = threshold.zip(average).is_some_and(|(limit, average)| average < limit);
            if threshold_failed {
                report.failures.push(format!("Score {average:?} is below the threshold {threshold:?}."));
            }
            let scores: Vec<Value> = scores
                .iter()
                .map(|score| json!({ "name": score.name, "score": score.score, "metadata": score.metadata }))
                .collect();
            report.eval = Some(json!({ "avgScore": average, "scores": scores, "thresholdFailed": threshold_failed }));
            report.run = Some(run);
        }
    }
    report
}

/// A case that builds its harness when it runs and then calls [`evaluate`].
pub fn describe_eval(
    harness: Arc<dyn Fn() -> anyhow::Result<Harness> + Send + Sync>,
    judges: Vec<Arc<dyn Judge>>,
    threshold: Option<f64>,
    input: Vec<Step>,
) -> CaseFn {
    Arc::new(move || {
        let (harness, judges, input) = (harness.clone(), judges.clone(), input.clone());
        Box::pin(async move { evaluate(harness(), &judges, threshold, &input).await })
    })
}

/// `(suite, case)` pairs of `project` in `files` (all when empty) whose
/// `"<suite> <case>"` name matches `pattern`.
pub fn select_cases<'a>(
    suites: &'a [EvalSuite],
    project: Project,
    files: &[String],
    pattern: Option<&Regex>,
) -> Vec<(&'a EvalSuite, &'a EvalCase)> {
    let mut selected = Vec::new();
    for suite in suites.iter().filter(|suite| suite.project == project) {
        if !files.is_empty() && !files.iter().any(|file| file.trim_start_matches("./") == suite.file) {
            continue;
        }
        for case in &suite.cases {
            if pattern.is_none_or(|pattern| pattern.is_match(&format!("{} {}", suite.name, case.name))) {
                selected.push((suite, case));
            }
        }
    }
    selected
}

/// Discovery output: `[{ name: "<suite> > <case>", file }]`.
pub fn list_cases(selected: &[(&EvalSuite, &EvalCase)]) -> Value {
    let items: Vec<Value> = selected
        .iter()
        .map(|(suite, case)| json!({ "name": format!("{} > {}", suite.name, case.name), "file": suite.file }))
        .collect();
    Value::Array(items)
}

/// Run cases one at a time and return the Vitest JSON report.
pub async fn run_cases(selected: &[(&EvalSuite, &EvalCase)]) -> Value {
    let start_time = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_millis() as u64);
    let mut files: Vec<(&str, Vec<Value>)> = Vec::new();
    let (mut passed, mut failed) = (0, 0);
    for (suite, case) in selected {
        let report = (case.run)().await;
        let status = if report.passed() { "passed" } else { "failed" };
        if report.passed() {
            passed += 1;
        } else {
            failed += 1;
        }
        println!("{} {} > {}", if report.passed() { "✓" } else { "✗" }, suite.name, case.name);
        for failure in &report.failures {
            println!("    {failure}");
        }
        let mut meta = json!({});
        if let Some(eval) = &report.eval {
            meta["eval"] = eval.clone();
        }
        if let Some(name) = &report.harness_name {
            meta["harness"] = json!({ "name": name });
            if let Some(run) = &report.run {
                meta["harness"]["run"] = run.to_json();
            }
        }
        let assertion = json!({
            "ancestorTitles": [suite.name], "fullName": format!("{} {}", suite.name, case.name), "status": status,
            "title": case.name, "failureMessages": report.failures, "meta": meta,
        });
        match files.iter_mut().find(|(file, _)| *file == suite.file) {
            Some((_, assertions)) => assertions.push(assertion),
            None => files.push((suite.file, vec![assertion])),
        }
    }
    let test_results: Vec<Value> = files
        .into_iter()
        .map(|(file, assertions)| {
            let status = if assertions.iter().all(|a| a["status"] == "passed") { "passed" } else { "failed" };
            json!({ "message": "", "name": file, "status": status, "assertionResults": assertions })
        })
        .collect();
    json!({
        "numFailedTests": failed, "numPassedTests": passed, "numPendingTests": 0, "numTodoTests": 0,
        "numTotalTests": passed + failed, "startTime": start_time, "success": failed == 0,
        "testResults": test_results,
    })
}
