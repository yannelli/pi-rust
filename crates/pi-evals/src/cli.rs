//! The documentation comparison (`eval:docs`), as upstream `cli.ts`.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::docker::{DockerContext, build_images, discover_cases, require_eval_auth_file, run_task};
use crate::framework::Project;
use crate::fsutil::{create_private_dir, sha256_hex};
use crate::harness::EnvMap;
use crate::plan::{DOCUMENTATION_VARIANTS, DiscoveredEvalCase, create_task_plan, parse_discovered_cases};
use crate::report::{
    EvalObservation, errored_observation, format_eval_comparison_report, read_task_observation,
    summarize_eval_observations,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvalCliOptions {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub runs_per_variant: u32,
    pub requested_files: Vec<String>,
    pub discovery_args: Vec<String>,
}

const RUNNER_OPTIONS: [&str; 3] = ["--provider", "--model", "--runs-per-variant"];
const FILTER_OPTIONS: [&str; 2] = ["-t", "--testNamePattern"];

fn trimmed(value: Option<&String>) -> Option<String> {
    value.map(|value| value.trim().to_string()).filter(|value| !value.is_empty())
}

pub fn parse_eval_cli(args: &[String], env: &EnvMap) -> anyhow::Result<EvalCliOptions> {
    let (mut provider, mut model, mut runs_text) = (None, None, None);
    let mut cli_selected_model = false;
    let (mut requested_files, mut discovery_args) = (Vec::new(), Vec::new());
    let mut index = 0;
    while index < args.len() {
        let argument = &args[index];
        index += 1;
        if argument.ends_with(".docs.eval.rs") {
            requested_files.push(argument.clone());
            continue;
        }
        if FILTER_OPTIONS.contains(&argument.as_str()) {
            let Some(value) = args.get(index).filter(|value| !value.is_empty()) else {
                anyhow::bail!("Missing value for {argument}.")
            };
            discovery_args.extend([argument.clone(), value.clone()]);
            index += 1;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--testNamePattern=") {
            if value.is_empty() {
                anyhow::bail!("Missing value for --testNamePattern.");
            }
            discovery_args.push(argument.clone());
            continue;
        }
        let (name, inline) = match argument.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (argument.as_str(), None),
        };
        if !RUNNER_OPTIONS.contains(&name) {
            anyhow::bail!("Unsupported eval argument: {argument}");
        }
        let separate = inline.is_none();
        let value = inline.or_else(|| args.get(index).cloned()).filter(|value| !value.is_empty());
        let Some(value) = value.filter(|value| !(separate && value.starts_with('-'))) else {
            anyhow::bail!("Missing value for {name}.")
        };
        match name {
            "--provider" => provider = Some(value),
            "--model" => model = Some(value),
            _ => runs_text = Some(value),
        }
        cli_selected_model |= name != "--runs-per-variant";
        if separate {
            index += 1;
        }
    }
    let (mut provider, mut model) = (trimmed(provider.as_ref()), trimmed(model.as_ref()));
    if cli_selected_model {
        if provider.is_none() || model.is_none() {
            anyhow::bail!("CLI model selection requires both --provider and --model.");
        }
    } else {
        provider = trimmed(env.get("PI_PROVIDER"));
        model = trimmed(env.get("PI_MODEL"));
        if provider.is_some() != model.is_some() {
            anyhow::bail!("Set both PI_PROVIDER and PI_MODEL, or neither.");
        }
    }
    let configured = trimmed(runs_text.as_ref().or_else(|| env.get("PI_EVAL_RUNS_PER_VARIANT")));
    let runs_per_variant = match configured {
        None => 1,
        Some(text) => match text.parse::<u32>() {
            Ok(runs) if runs >= 1 => runs,
            _ => anyhow::bail!("Runs per variant must be a positive integer."),
        },
    };
    Ok(EvalCliOptions { provider, model, runs_per_variant, requested_files, discovery_args })
}

/// Days since the epoch as a civil `(year, month, day)`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

/// `<ISO timestamp with ':' as '-'>_<random UUID>`.
pub fn artifact_run_id() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let (seconds, millis) = (now.as_secs() as i64, now.subsec_millis());
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    let time = seconds.rem_euclid(86_400);
    let mut bytes = [0u8; 16];
    let _ = ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}-{:02}-{:02}.{millis:03}Z_{}-{}-{}-{}-{}",
        time / 3600,
        time % 3600 / 60,
        time % 60,
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// A requested file as the documentation suite registry names it.
pub fn container_path(path: &str, docs_files: &[&str]) -> anyhow::Result<String> {
    let normalized = path.replace('\\', "/");
    let normalized = normalized.trim_start_matches("./");
    let normalized = normalized.strip_prefix("crates/pi-evals/").unwrap_or(normalized);
    if !normalized.ends_with(".docs.eval.rs") || !docs_files.contains(&normalized) {
        anyhow::bail!("Documentation runner cannot execute non-doc eval: {path}");
    }
    Ok(normalized.to_string())
}

pub fn compare_discovery(left: &[DiscoveredEvalCase], right: &[DiscoveredEvalCase]) -> anyhow::Result<()> {
    let identities = |cases: &[DiscoveredEvalCase]| {
        let mut ids: Vec<(String, String)> = cases.iter().map(|c| (c.full_name.clone(), c.file.clone())).collect();
        ids.sort();
        ids
    };
    if identities(left) != identities(right) {
        anyhow::bail!("Documentation variants discovered different eval cases.");
    }
    Ok(())
}

fn write_json(path: &Path, value: &impl serde::Serialize) -> anyhow::Result<()> {
    std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(value)?))?;
    Ok(())
}

/// Run the whole comparison; returns whether every pair was eligible.
pub fn run_docs_comparison(args: &[String], env: &EnvMap) -> anyhow::Result<bool> {
    let cli = parse_eval_cli(args, env)?;
    let (Some(provider), Some(model)) = (cli.provider.clone(), cli.model.clone()) else {
        anyhow::bail!("Set PI_PROVIDER and PI_MODEL, or pass --provider and --model.")
    };
    let model_identity = format!("{provider}/{model}");
    let suites = crate::evals::all_suites();
    let docs_files: Vec<&str> =
        suites.iter().filter(|suite| suite.project == Project::Docs).map(|suite| suite.file).collect();
    let requested: Vec<String> = if cli.requested_files.is_empty() {
        docs_files.iter().map(|file| file.to_string()).collect()
    } else {
        cli.requested_files.clone()
    };
    let mut files =
        requested.iter().map(|file| container_path(file, &docs_files)).collect::<anyhow::Result<Vec<_>>>()?;
    files.sort();
    if files.is_empty() {
        anyhow::bail!("No documentation eval files were selected.");
    }
    let artifact_directory: PathBuf = crate::docker::package_root().join(".eval").join(artifact_run_id());
    let auth_path = require_eval_auth_file(&provider, env)?;
    create_private_dir(&artifact_directory)?;
    let images = build_images(&crate::evals::repository_root())?;
    if images.with_docs.id == images.without_docs.id {
        anyhow::bail!("Documentation variants resolved to the same image.");
    }
    let docker = DockerContext {
        images: images.clone(),
        artifact_directory: artifact_directory.clone(),
        auth_path,
        provider,
        model,
        runs_per_variant: cli.runs_per_variant,
    };
    let mut discoveries = Vec::new();
    for variant in DOCUMENTATION_VARIANTS {
        let path = discover_cases(&docker, variant, &files, &cli.discovery_args)?;
        discoveries.push(parse_discovered_cases(&serde_json::from_str::<Value>(&std::fs::read_to_string(path)?)?)?);
    }
    compare_discovery(&discoveries[0], &discoveries[1])?;
    let cases = discoveries.swap_remove(0);
    if cases.is_empty() {
        anyhow::bail!("No documentation eval cases matched the selection.");
    }
    let tasks = create_task_plan(&cases, &model_identity, cli.runs_per_variant)?;
    let case_summaries: Vec<Value> =
        cases.iter().map(|c| json!({ "evalSet": c.eval_set, "caseId": c.case_id, "file": c.file })).collect();
    let mut protocol = json!({
        "schemaVersion": 1, "model": model_identity, "runsPerVariant": cli.runs_per_variant, "images": images,
        "files": files, "cases": case_summaries, "tasks": tasks,
    });
    let protocol_digest = sha256_hex(protocol.to_string().as_bytes());
    protocol["protocolDigest"] = json!(protocol_digest);
    write_json(&artifact_directory.join("protocol.json"), &protocol)?;
    write_json(&artifact_directory.join("expected-runs.json"), &tasks)?;
    let mut observations: Vec<EvalObservation> = Vec::new();
    for (index, task) in tasks.iter().enumerate() {
        println!(
            "\n[{}/{}] {} > {} | {} | {} | run {}",
            index + 1,
            tasks.len(),
            task.eval_set,
            task.case_id,
            task.variant,
            task.model,
            task.run_number
        );
        let observation = match run_task(&docker, task)? {
            Some(report) => read_task_observation(task, &report, &artifact_directory),
            None => errored_observation(task),
        };
        observations.push(observation);
        let lines: Vec<String> = observations.iter().map(|item| json!(item).to_string()).collect();
        std::fs::write(artifact_directory.join("observations.jsonl"), format!("{}\n", lines.join("\n")))?;
    }
    let expected: Vec<_> = tasks.iter().map(Into::into).collect();
    let report = summarize_eval_observations(&protocol_digest, &expected, &observations);
    let report_text = format_eval_comparison_report(&report);
    write_json(&artifact_directory.join("report.json"), &report)?;
    std::fs::write(artifact_directory.join("report.txt"), format!("{report_text}\n"))?;
    println!("\n{report_text}");
    println!("\nArtifacts: {}", artifact_directory.display());
    Ok(report.blocked_pairs.is_empty())
}
