//! Host eval: audit each documentation page against the implementation. pi-rust has no
//! packaged docs directory, so the pages are the repository and crate READMEs plus any
//! `crates/pi-coding-agent/docs/**/*.md`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::repository_root;
use crate::framework::{CaseReport, EvalCase, EvalSuite, Project};
use crate::harness::{HarnessOptions, Step, create_pi_coding_agent_harness};

const TOOL_NAME: &str = "submit_documentation_audit";
const SUBMIT_AUDIT_EXTENSION: &str = include_str!("submit-documentation-audit.ts");

fn markdown_files(directory: &Path, root: &Path, pages: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(directory) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            markdown_files(&path, root, pages);
        } else if path.extension().is_some_and(|extension| extension == "md") {
            let relative = path.strip_prefix(root).unwrap_or(&path);
            pages.push(relative.display().to_string().replace('\\', "/"));
        }
    }
}

/// Documentation pages relative to the repository root, sorted.
pub fn documentation_pages(root: &Path) -> Vec<String> {
    let mut pages = vec!["README.md".to_string()];
    if root.join("crates/pi-coding-agent/README.md").is_file() {
        pages.push("crates/pi-coding-agent/README.md".into());
    }
    markdown_files(&root.join("crates/pi-coding-agent/docs"), root, &mut pages);
    pages.sort();
    pages
}

async fn audit(root: PathBuf, page: String) -> CaseReport {
    let options = HarnessOptions {
        name: Some("documentation-page-audit".into()),
        tools: Some(["read", "grep", "find", "ls", TOOL_NAME].map(String::from).to_vec()),
        extensions: vec![("submit-documentation-audit.ts".into(), SUBMIT_AUDIT_EXTENSION.into())],
        ..HarnessOptions::default()
    };
    let harness = create_pi_coding_agent_harness(options);
    let mut report = CaseReport { harness_name: Some(harness.name.clone()), ..CaseReport::default() };
    let documentation = root.join(&page);
    let prompt = format!(
        "Audit this Pi documentation page against the repository implementation.

Documentation page: {}
Repository root: {}

Read the complete page and the relevant implementation. Report a mismatch only for a clear, user-visible contradiction between an explicit documentation claim and actual runtime behavior. Follow the runtime path; names, comments, types, isolated helpers, and tests are not sufficient evidence by themselves.

Missing internal detail, ambiguous wording, hypothetical misuse, and undocumented edge cases are not mismatches. If the evidence is not decisive, report inconclusive. Otherwise report match.

For a mismatch, quote the claim, cite the implementation path and symbol, and state the concrete behavior a user would observe.

Call {TOOL_NAME} exactly once as your final action. Do not return prose.",
        documentation.display(),
        root.display()
    );
    match harness.run(&[Step::Prompt(prompt)]).await {
        Err(failure) => {
            report.failures.push(failure.error);
            report.run = failure.partial.map(|run| *run);
        }
        Ok(run) => {
            let calls: Vec<_> = run.tool_calls().into_iter().filter(|call| call.name == TOOL_NAME).collect();
            report.check(calls.len() == 1, format!("expected 1 {TOOL_NAME} call, got {}", calls.len()));
            if let Some(call) = calls.first() {
                report.check(call.ok, "the audit call failed");
                let explanation = call.arguments["explanation"].as_str().unwrap_or_default();
                report.check(call.arguments["verdict"] == "match", format!("verdict is not match: {explanation}"));
            }
            report.run = Some(run);
        }
    }
    report
}

pub fn suite() -> EvalSuite {
    let root = repository_root();
    let cases = documentation_pages(&root)
        .into_iter()
        .map(|page| {
            let root = root.clone();
            EvalCase {
                name: format!("{page} matches the implementation"),
                run: Arc::new(move || Box::pin(audit(root.clone(), page.clone()))),
            }
        })
        .collect();
    EvalSuite {
        file: "evals/documentation-audit.eval.rs",
        project: Project::Host,
        name: "Audit documentation against implementation".into(),
        cases,
    }
}
