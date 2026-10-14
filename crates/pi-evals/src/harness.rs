//! The pi coding-agent eval harness, as upstream `harness.ts`. Each prompt step runs the
//! Rust `pi` binary in print mode against an isolated home, agent directory, workspace,
//! and session; a reload step starts the next prompt in a fresh process.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use futures::future::BoxFuture;
use pi_ai::Message;
use serde_json::Value;

use crate::plan::DocumentationVariant;

pub use crate::harness_run::{HarnessFailure, HarnessRun, ToolCallRecord};

pub type EnvMap = BTreeMap<String, String>;

/// A snapshot of the process environment; harness functions read this, never the live one.
pub fn process_env() -> EnvMap {
    std::env::vars().collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelSelection {
    pub provider: String,
    pub id: String,
}

pub fn resolve_model_selection(explicit: Option<&ModelSelection>, env: &EnvMap) -> anyhow::Result<ModelSelection> {
    let pick = |explicit: Option<&String>, name: &str| {
        explicit.or_else(|| env.get(name)).map(|value| value.trim().to_string()).filter(|value| !value.is_empty())
    };
    let provider = pick(explicit.map(|model| &model.provider), "PI_PROVIDER");
    let id = pick(explicit.map(|model| &model.id), "PI_MODEL");
    match (provider, id) {
        (Some(provider), Some(id)) => Ok(ModelSelection { provider, id }),
        _ => anyhow::bail!("Select a harness model explicitly or set both PI_PROVIDER and PI_MODEL as defaults."),
    }
}

/// Environment changes for an agent process: runner metadata (`PI_EVAL_*`) is removed and
/// the home and agent directory point into the isolated run root.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EnvironmentPlan {
    pub set: Vec<(String, String)>,
    pub remove: Vec<String>,
}

impl EnvironmentPlan {
    pub fn apply(&self, command: &mut Command) {
        for name in &self.remove {
            command.env_remove(name);
        }
        for (name, value) in &self.set {
            command.env(name, value);
        }
    }

    /// `env` as the agent process sees it.
    pub fn applied_to(&self, env: &EnvMap) -> EnvMap {
        let mut result = env.clone();
        for name in &self.remove {
            result.remove(name);
        }
        result.extend(self.set.iter().cloned());
        result
    }
}

pub fn isolated_environment(home: &str, agent_dir: &str, env: &EnvMap) -> EnvironmentPlan {
    let remove = env.keys().filter(|name| name.starts_with("PI_EVAL_")).cloned().collect();
    let set = [("HOME", home), ("USERPROFILE", home), ("PI_CODING_AGENT_DIR", agent_dir)]
        .into_iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    EnvironmentPlan { set, remove }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SandboxIdentity {
    pub uid: u32,
    pub gid: u32,
}

fn parse_sandbox_id(env: &EnvMap, name: &str) -> anyhow::Result<Option<u32>> {
    let Some(value) = env.get(name) else { return Ok(None) };
    match value.parse::<u32>() {
        Ok(id) if id >= 1 => Ok(Some(id)),
        _ => anyhow::bail!("{name} must be a positive integer."),
    }
}

pub fn resolve_sandbox_identity(env: &EnvMap) -> anyhow::Result<Option<SandboxIdentity>> {
    match (parse_sandbox_id(env, "PI_EVAL_SANDBOX_UID")?, parse_sandbox_id(env, "PI_EVAL_SANDBOX_GID")?) {
        (None, None) => Ok(None),
        (Some(uid), Some(gid)) => Ok(Some(SandboxIdentity { uid, gid })),
        _ => anyhow::bail!("Set both PI_EVAL_SANDBOX_UID and PI_EVAL_SANDBOX_GID, or neither."),
    }
}

pub fn verify_system_prompt(prompt: &str, name: &str, expected_documentation: Option<bool>) -> anyhow::Result<String> {
    let Some(expected) = expected_documentation else { return Ok(prompt.to_string()) };
    if !prompt.contains("\n<rules>\n") {
        anyhow::bail!("Pi system prompt lost its rules in the {name} eval variant.");
    }
    if prompt.contains("\n<docs>\nPi documentation (read only") != expected {
        anyhow::bail!("Pi system prompt does not match the {name} eval variant.");
    }
    Ok(prompt.to_string())
}

/// Documentation evals intentionally exclude shell and unrestricted network tools.
pub const DOCUMENTATION_EVAL_TOOLS: [&str; 6] = ["read", "write", "edit", "grep", "find", "ls"];

pub fn resolve_documentation_variant(value: Option<&str>) -> anyhow::Result<DocumentationVariant> {
    value
        .and_then(DocumentationVariant::parse)
        .ok_or_else(|| anyhow::anyhow!("PI_EVAL_VARIANT must be \"without_docs\" or \"with_docs\"."))
}

pub fn exclude_pi_documentation(prompt: &str) -> anyhow::Result<String> {
    const START: &str = "\n<docs>\n";
    const END: &str = "\n</docs>";
    let Some(start) = prompt.find(START) else {
        anyhow::bail!("Default Pi system prompt has no Pi documentation section.")
    };
    let Some(end) = prompt[start..].find(END).map(|offset| start + offset) else {
        anyhow::bail!("Default Pi system prompt has no complete Pi documentation section.")
    };
    if prompt.rfind("\n<cwd>\n").is_none_or(|cwd| cwd < end) {
        anyhow::bail!("Default Pi system prompt has no working-directory section.");
    }
    Ok(format!("{}{}", &prompt[..start], &prompt[end + END.len()..]))
}

/// What a harness `output` function sees after the last step.
pub struct OutputContext {
    pub response: String,
    pub system_prompt: String,
    pub agent_dir: PathBuf,
    pub workspace: PathBuf,
    pub session_file: PathBuf,
    pub messages: Vec<Message>,
    /// The agent process environment, for oracles that load the configured runtime.
    pub env: EnvMap,
}

pub type OutputFn = Arc<dyn Fn(OutputContext) -> BoxFuture<'static, anyhow::Result<Value>> + Send + Sync>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Prompt(String),
    Reload,
}

#[derive(Clone, Default)]
pub struct HarnessOptions {
    pub name: Option<String>,
    pub model: Option<ModelSelection>,
    pub no_tools: bool,
    pub tools: Option<Vec<String>>,
    /// Extension modules `(file name, source)` passed with `-e`, such as custom tools.
    pub extensions: Vec<(String, String)>,
    pub workspace_files: Vec<(String, String)>,
    /// Extra variables for the agent process.
    pub env: Vec<(String, String)>,
    pub exclude_pi_documentation: bool,
    pub expected_pi_documentation: Option<bool>,
    pub output: Option<OutputFn>,
}

#[derive(Clone)]
pub struct Harness {
    pub name: String,
    pub options: HarnessOptions,
}

impl Harness {
    pub async fn run(&self, steps: &[Step]) -> Result<HarnessRun, HarnessFailure> {
        crate::harness_run::run_pi_coding_agent(steps, &self.options, process_env()).await
    }
}

pub fn create_pi_coding_agent_harness(options: HarnessOptions) -> Harness {
    Harness { name: options.name.clone().unwrap_or_else(|| "pi-coding-agent".into()), options }
}

const PROMPT_TRANSFORM_EXTENSION: &str = include_str!("../evals/eval-system-prompt-transform.ts");

/// A harness for one documentation variant; only valid inside the container sandbox.
pub fn create_pi_documentation_eval_harness(mut options: HarnessOptions, env: &EnvMap) -> anyhow::Result<Harness> {
    if env.get("PI_EVAL_CONTAINER").map(String::as_str) != Some("1") || resolve_sandbox_identity(env)?.is_none() {
        anyhow::bail!("Documentation evals must run in the isolated container sandbox.");
    }
    let variant = resolve_documentation_variant(env.get("PI_EVAL_VARIANT").map(String::as_str))?;
    options.name = Some(variant.as_str().into());
    options.tools.get_or_insert_with(|| DOCUMENTATION_EVAL_TOOLS.iter().map(|tool| tool.to_string()).collect());
    if variant == DocumentationVariant::WithoutDocs {
        options.exclude_pi_documentation = true;
        let transform = ("eval-system-prompt-transform.ts".to_string(), PROMPT_TRANSFORM_EXTENSION.to_string());
        options.extensions.push(transform);
    }
    options.expected_pi_documentation = Some(variant == DocumentationVariant::WithDocs);
    Ok(create_pi_coding_agent_harness(options))
}
