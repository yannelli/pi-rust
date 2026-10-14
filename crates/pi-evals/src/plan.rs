//! Expands discovered cases into `(case, variant, model, run)` tasks, as upstream `plan.ts`.

use std::collections::HashSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum DocumentationVariant {
    #[serde(rename = "without_docs")]
    WithoutDocs,
    #[serde(rename = "with_docs")]
    WithDocs,
}

pub const DOCUMENTATION_VARIANTS: [DocumentationVariant; 2] =
    [DocumentationVariant::WithoutDocs, DocumentationVariant::WithDocs];

impl DocumentationVariant {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WithoutDocs => "without_docs",
            Self::WithDocs => "with_docs",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "without_docs" => Some(Self::WithoutDocs),
            "with_docs" => Some(Self::WithDocs),
            _ => None,
        }
    }
}

impl fmt::Display for DocumentationVariant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredEvalCase {
    pub file: String,
    pub full_name: String,
    pub eval_set: String,
    pub case_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalTask {
    pub file: String,
    pub full_name: String,
    pub eval_set: String,
    pub case_id: String,
    pub variant: DocumentationVariant,
    pub model: String,
    pub run_number: u32,
}

/// Parse `[{ name, file }]` discovery output; names must be `<eval set> > <case>`.
pub fn parse_discovered_cases(value: &Value) -> anyhow::Result<Vec<DiscoveredEvalCase>> {
    let Some(items) = value.as_array() else { anyhow::bail!("Discovered eval cases must be an array.") };
    let mut identities = HashSet::new();
    let mut cases = Vec::with_capacity(items.len());
    for item in items {
        let (Some(name), Some(file)) = (item["name"].as_str(), item["file"].as_str()) else {
            anyhow::bail!("Discovered eval case is invalid.");
        };
        let parts: Vec<&str> = name.split(" > ").collect();
        if parts.len() != 2 || parts[0].trim().is_empty() || parts[1].trim().is_empty() {
            anyhow::bail!("Documentation eval must use \"<eval set> > <case>\": {name}");
        }
        if !identities.insert((parts[0].to_string(), parts[1].to_string())) {
            anyhow::bail!("Duplicate eval case identity: {name}");
        }
        cases.push(DiscoveredEvalCase {
            file: file.to_string(),
            full_name: name.to_string(),
            eval_set: parts[0].to_string(),
            case_id: parts[1].to_string(),
        });
    }
    Ok(cases)
}

/// One task per case, variant, and repetition; odd runs start with `without_docs`.
pub fn create_task_plan(
    cases: &[DiscoveredEvalCase],
    model: &str,
    runs_per_variant: u32,
) -> anyhow::Result<Vec<EvalTask>> {
    if !model.contains('/') || model.starts_with('/') || model.ends_with('/') {
        anyhow::bail!("Model identity must contain a provider and model.");
    }
    if runs_per_variant < 1 {
        anyhow::bail!("Runs per variant must be a positive integer.");
    }
    let mut tasks = Vec::new();
    for case in cases {
        for run_number in 1..=runs_per_variant {
            let variants = if run_number % 2 == 1 {
                DOCUMENTATION_VARIANTS
            } else {
                [DocumentationVariant::WithDocs, DocumentationVariant::WithoutDocs]
            };
            for variant in variants {
                tasks.push(EvalTask {
                    file: case.file.clone(),
                    full_name: case.full_name.clone(),
                    eval_set: case.eval_set.clone(),
                    case_id: case.case_id.clone(),
                    variant,
                    model: model.to_string(),
                    run_number,
                });
            }
        }
    }
    Ok(tasks)
}
