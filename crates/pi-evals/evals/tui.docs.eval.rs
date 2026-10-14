//! Documentation eval: replace the footer's numeric context usage with a progress bar.

use std::sync::Arc;

use regex::Regex;
use serde_json::json;

use super::run_local;
use super::tui_oracle::{BUILT_IN_CONTEXT_PATTERN, CONTEXT_FIXTURES, CONTEXT_WINDOW, inspect_context_footer, lines};
use crate::framework::{EvalCase, EvalSuite, Judge, JudgeScore, Project, describe_eval};
use crate::harness::{
    HarnessOptions, HarnessRun, OutputContext, Step, create_pi_documentation_eval_harness, process_env,
};

/// Normalized Levenshtein similarity, as `autoevals` `Levenshtein`: 1 - distance / longest.
pub fn levenshtein_similarity(expected: &str, output: &str) -> f64 {
    let (a, b): (Vec<char>, Vec<char>) = (expected.chars().collect(), output.chars().collect());
    let longest = a.len().max(b.len());
    if longest == 0 {
        return 1.0;
    }
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    for (i, left) in a.iter().enumerate() {
        let mut current = vec![i + 1];
        for (j, right) in b.iter().enumerate() {
            let substitution = previous[j] + usize::from(left != right);
            current.push(substitution.min(previous[j + 1] + 1).min(current[j] + 1));
        }
        previous = current;
    }
    1.0 - previous[b.len()] as f64 / longest as f64
}

fn normalize(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Scores 1 only when every fixture renders its exact bar, without the built-in context text.
pub struct ContextFooterJudge;

impl Judge for ContextFooterJudge {
    fn judge(&self, _input: &[Step], run: &HarnessRun) -> JudgeScore {
        let output = &run.output;
        let pattern = Regex::new(BUILT_IN_CONTEXT_PATTERN).expect("valid pattern");
        let marker = "{context}";
        let mut valid = output["extensionErrors"].as_array().is_some_and(Vec::is_empty);
        let reference = output["builtInStatusLine"]
            .as_str()
            .map(|line| pattern.replace(&normalize(line), marker).into_owned())
            .filter(|reference| reference.contains(marker));
        valid &= reference.is_some();
        let mut similarities = Vec::new();
        for (index, (percent, bar)) in CONTEXT_FIXTURES.iter().enumerate() {
            let observation = &output["observations"][index];
            if observation["percent"].as_f64() != Some(*percent) {
                valid = false;
                continue;
            }
            let usage = &observation["contextUsage"];
            let rounded = (usage["percent"].as_f64().unwrap_or(-1.0) * 10.0).round() / 10.0;
            if usage["contextWindow"].as_u64() != Some(CONTEXT_WINDOW) || rounded != *percent {
                valid = false;
            }
            let rendered = observation["renderedOutput"].as_str().unwrap_or_default();
            let Some(line) = lines(rendered).into_iter().find(|line| line.contains(bar) || pattern.is_match(line))
            else {
                valid = false;
                continue;
            };
            let line = normalize(line);
            if !line.contains(bar) || pattern.is_match(&line) {
                valid = false;
                continue;
            }
            if CONTEXT_FIXTURES.iter().any(|(_, other)| other != bar && line.contains(other)) {
                valid = false;
            }
            if let Some(reference) = &reference {
                similarities.push(levenshtein_similarity(reference, &line.replacen(bar, marker, 1)));
            }
        }
        let retained = if similarities.len() == CONTEXT_FIXTURES.len() {
            similarities.iter().copied().fold(f64::INFINITY, f64::min)
        } else {
            0.0
        };
        let retained_percent = (retained * 100.0).round();
        let rationale = if valid {
            format!("Footer retained {retained_percent}% of the built-in status text.")
        } else {
            "Context progress behavior was incorrect.".to_string()
        };
        JudgeScore {
            name: "ContextFooterJudge".into(),
            score: Some(if valid { 1.0 } else { 0.0 }),
            metadata: json!({ "rationale": rationale, "retainedPercent": retained_percent }),
        }
    }
}

pub fn suite() -> EvalSuite {
    let harness = Arc::new(|| {
        let output = Arc::new(|context: OutputContext| {
            Box::pin(run_local(
                move || async move { inspect_context_footer(&context.agent_dir, &context.workspace).await },
            )) as _
        });
        create_pi_documentation_eval_harness(
            HarnessOptions { output: Some(output), ..HarnessOptions::default() },
            &process_env(),
        )
    });
    let judges: Vec<Arc<dyn Judge>> = vec![Arc::new(ContextFooterJudge)];
    let prompt = "Change the Pi footer replacing the built-in context info, e.g. `42.2%/272k (auto)`, with a ten-cell progress bar to make it show ████░░░░░░ 42% instead. Clamp percentages to the 0-100 range. Just replace the context. Otherwise leave everything else the same.";
    EvalSuite {
        file: "evals/tui.docs.eval.rs",
        project: Project::Docs,
        name: "Customize the interactive context footer".into(),
        cases: vec![EvalCase {
            name: "replaces numeric context usage with a progress bar".into(),
            run: describe_eval(harness, judges, None, vec![Step::Prompt(prompt.into()), Step::Reload]),
        }],
    }
}
