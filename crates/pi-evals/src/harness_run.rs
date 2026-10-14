//! One harness run: seeds an isolated root, runs `pi` once per prompt step, and reads the
//! session file back into transcript events, usage, and the sent system prompt.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use pi_ai::{Message, StopReason};
use serde_json::{Value, json};

use crate::fsutil::{create_private_dir, sha256_hex, write_private};
use crate::harness::{
    EnvMap, HarnessOptions, OutputContext, SandboxIdentity, Step, exclude_pi_documentation, isolated_environment,
    resolve_model_selection, resolve_sandbox_identity, verify_system_prompt,
};
use crate::report::PI_SESSION_SNAPSHOT_ARTIFACT;
use crate::session::{SessionSnapshot, content_text, read_session};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct HarnessRun {
    pub output: Value,
    pub events: Vec<Value>,
    pub usage: Value,
    pub metadata: Value,
    pub total_ms: f64,
    pub artifacts: BTreeMap<String, String>,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolCallRecord {
    pub name: String,
    pub arguments: Value,
    pub ok: bool,
}

impl HarnessRun {
    /// The normalized run as a report's `meta.harness.run`.
    pub fn to_json(&self) -> Value {
        json!({
            "output": self.output,
            "session": { "events": self.events, "metadata": self.metadata },
            "usage": self.usage,
            "timings": { "totalMs": crate::fsutil::json_number(self.total_ms.round()) },
            "artifacts": self.artifacts,
            "errors": self.errors.iter().map(|message| json!({ "message": message })).collect::<Vec<_>>(),
        })
    }

    /// Tool calls paired with their results, in call order.
    pub fn tool_calls(&self) -> Vec<ToolCallRecord> {
        let calls = self.events.iter().filter(|event| event["type"] == "tool_call");
        calls
            .map(|call| {
                let result = self
                    .events
                    .iter()
                    .find(|event| event["type"] == "tool_result" && event["toolCallId"] == call["id"]);
                ToolCallRecord {
                    name: call["name"].as_str().unwrap_or_default().to_string(),
                    arguments: call["arguments"].clone(),
                    ok: result.is_some_and(|result| result.get("error").is_none()),
                }
            })
            .collect()
    }
}

/// A failed run, with the partial run when the agent produced diagnostics.
#[derive(Debug)]
pub struct HarnessFailure {
    pub error: String,
    pub partial: Option<Box<HarnessRun>>,
}

fn pi_binary(env: &EnvMap) -> PathBuf {
    if let Some(path) = env.get("PI_EVAL_PI_BIN") {
        return PathBuf::from(path);
    }
    let name = if cfg!(windows) { "pi.exe" } else { "pi" };
    let sibling = std::env::current_exe().ok().and_then(|exe| exe.parent().map(|dir| dir.join(name)));
    sibling.filter(|path| path.is_file()).unwrap_or_else(|| PathBuf::from(name))
}

fn seed_workspace(workspace: &Path, files: &[(String, String)]) -> anyhow::Result<()> {
    for (name, content) in files {
        let relative = Path::new(name);
        if name.is_empty() || relative.is_absolute() || relative.has_root() {
            anyhow::bail!("Invalid workspace fixture path: {name}");
        }
        if relative.components().any(|part| matches!(part, Component::ParentDir | Component::Prefix(_))) {
            anyhow::bail!("Workspace fixture escapes the workspace: {name}");
        }
        let path = workspace.join(relative);
        std::fs::create_dir_all(path.parent().unwrap_or(workspace))?;
        write_private(&path, content.as_bytes())?;
    }
    Ok(())
}

/// Copy only the selected provider's credential from the host `auth.json`.
fn seed_credentials(host_agent_dir: &Path, agent_dir: &Path, provider: &str, sandboxed: bool) -> anyhow::Result<()> {
    let host_auth = host_agent_dir.join("auth.json");
    let Ok(text) = std::fs::read_to_string(&host_auth) else { return Ok(()) };
    let credentials: Value = serde_json::from_str(&text)?;
    if let Some(credential) = credentials.get(provider) {
        let selected = json!({ provider: credential });
        write_private(&agent_dir.join("auth.json"), serde_json::to_string_pretty(&selected)?.as_bytes())?;
    }
    if sandboxed {
        std::fs::remove_file(&host_auth).ok();
    }
    Ok(())
}

#[cfg(unix)]
fn enter_sandbox(command: &mut Command, root: &Path, identity: Option<SandboxIdentity>) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    fn chown_tree(path: &Path, identity: SandboxIdentity) -> std::io::Result<()> {
        let metadata = std::fs::symlink_metadata(path)?;
        if metadata.is_dir() {
            for entry in std::fs::read_dir(path)? {
                chown_tree(&entry?.path(), identity)?;
            }
        }
        std::os::unix::fs::lchown(path, Some(identity.uid), Some(identity.gid))
    }
    if let Some(identity) = identity {
        chown_tree(root, identity)?;
        command.uid(identity.uid).gid(identity.gid);
    }
    Ok(())
}

#[cfg(not(unix))]
fn enter_sandbox(_command: &mut Command, _root: &Path, identity: Option<SandboxIdentity>) -> anyhow::Result<()> {
    if identity.is_some() {
        anyhow::bail!("The eval filesystem sandbox requires POSIX user APIs.");
    }
    Ok(())
}

struct RunPaths {
    root: PathBuf,
    workspace: PathBuf,
    home: PathBuf,
    agent_dir: PathBuf,
    sessions: PathBuf,
    extensions: PathBuf,
}

fn find_session_file(directory: &Path) -> Option<PathBuf> {
    for entry in std::fs::read_dir(directory).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_session_file(&path) {
                return Some(found);
            }
        } else if path.extension().is_some_and(|extension| extension == "jsonl") {
            return Some(path);
        }
    }
    None
}

fn last_assistant_text(messages: &[Message]) -> anyhow::Result<String> {
    let Some(assistant) = messages.iter().rev().find_map(|message| match message {
        Message::Assistant(assistant) => Some(assistant),
        _ => None,
    }) else {
        anyhow::bail!("Agent run completed without an assistant message.")
    };
    if !matches!(assistant.stop_reason, StopReason::Stop | StopReason::ToolUse) {
        let reason = serde_json::to_value(assistant.stop_reason).unwrap_or_default();
        let fallback = format!("Agent run ended with unexpected stop reason: {}.", reason.as_str().unwrap_or("?"));
        anyhow::bail!(assistant.error_message.clone().unwrap_or(fallback));
    }
    let text = content_text(&assistant.content);
    if text.is_empty() && assistant.stop_reason == StopReason::Stop {
        anyhow::bail!("Agent run produced no assistant text.");
    }
    Ok(text)
}

struct Prepared {
    paths: RunPaths,
    snapshot: Option<SessionSnapshot>,
    session_file: Option<PathBuf>,
}

async fn run_steps(
    steps: &[Step],
    options: &HarnessOptions,
    env: &EnvMap,
    state: &mut Prepared,
) -> anyhow::Result<(String, String, EnvMap)> {
    let selection = resolve_model_selection(options.model.as_ref(), env)?;
    let host_agent_dir = env.get("PI_CODING_AGENT_DIR").map(PathBuf::from).unwrap_or_else(|| {
        let home = env.get("HOME").or_else(|| env.get("USERPROFILE")).cloned().unwrap_or_default();
        Path::new(&home).join(".pi").join("agent")
    });
    let sandbox = resolve_sandbox_identity(env)?;
    let paths = &state.paths;
    for directory in [&paths.workspace, &paths.agent_dir, &paths.sessions, &paths.extensions] {
        create_private_dir(directory)?;
    }
    seed_workspace(&paths.workspace, &options.workspace_files)?;
    seed_credentials(&host_agent_dir, &paths.agent_dir, &selection.provider, sandbox.is_some())?;
    let mut extension_args = Vec::new();
    for (name, source) in &options.extensions {
        let path = paths.extensions.join(name);
        write_private(&path, source.as_bytes())?;
        extension_args.extend(["-e".to_string(), path.display().to_string()]);
    }
    let plan = isolated_environment(&paths.home.display().to_string(), &paths.agent_dir.display().to_string(), env);
    let child_env = plan.applied_to(env).into_iter().chain(options.env.iter().cloned()).collect::<EnvMap>();
    let mut response = None;
    for step in steps {
        let Step::Prompt(prompt) = step else { continue };
        let mut command = Command::new(pi_binary(env));
        command.current_dir(&paths.workspace).stdin(Stdio::null()).stdout(Stdio::null());
        command.args(["--provider", &selection.provider, "--model", &selection.id, "--thinking", "off"]);
        match &state.session_file {
            Some(file) => command.arg("--session").arg(file),
            None => command.arg("--session-dir").arg(&paths.sessions),
        };
        if options.no_tools {
            command.arg("--no-tools");
        } else if let Some(tools) = &options.tools {
            command.args(["--tools", &tools.join(",")]);
        }
        command.args(&extension_args).arg("-p").arg(prompt);
        plan.apply(&mut command);
        command.envs(options.env.iter().cloned());
        enter_sandbox(&mut command, &paths.root, sandbox)?;
        let status = tokio::task::spawn_blocking(move || command.status()).await??;
        state.session_file = state.session_file.take().or_else(|| find_session_file(&paths.sessions));
        let Some(file) = &state.session_file else { anyhow::bail!("pi exited with {status} and wrote no session.") };
        let previous = state.snapshot.as_ref().map_or(0, |snapshot| snapshot.messages.len());
        let snapshot = read_session(file)?;
        let text = last_assistant_text(snapshot.messages.get(previous..).unwrap_or_default());
        state.snapshot = Some(snapshot);
        response = Some(text?);
    }
    let Some(response) = response else { anyhow::bail!("Pi eval input must include at least one prompt step.") };
    let messages = &state.snapshot.as_ref().map(|snapshot| snapshot.messages.clone()).unwrap_or_default();
    let mut system_prompt = pi_ai::transcript::current_system_prompt(messages);
    if options.exclude_pi_documentation {
        system_prompt = exclude_pi_documentation(&system_prompt)?;
    }
    Ok((response, system_prompt, child_env))
}

pub(crate) async fn run_pi_coding_agent(
    steps: &[Step],
    options: &HarnessOptions,
    env: EnvMap,
) -> Result<HarnessRun, HarnessFailure> {
    let started = Instant::now();
    let root = match tempfile::Builder::new().prefix("pi-eval-").tempdir() {
        Ok(root) => root.keep(),
        Err(error) => return Err(HarnessFailure { error: error.to_string(), partial: None }),
    };
    let home = root.join("home");
    let paths = RunPaths {
        workspace: root.join("workspace"),
        agent_dir: home.join(".pi").join("agent"),
        sessions: root.join("sessions"),
        extensions: root.join("extensions"),
        home,
        root,
    };
    let mut state = Prepared { paths, snapshot: None, session_file: None };
    let outcome = run_steps(steps, options, &env, &mut state).await;
    let mut run = state.snapshot.as_ref().map(|snapshot| diagnostics(snapshot, &state.paths.agent_dir));
    let mut errors = Vec::new();
    let mut result = Value::Null;
    match outcome {
        Ok((response, system_prompt, child_env)) => {
            if let Some(run) = &mut run {
                run.metadata = json!({ "systemPromptSha256": sha256_hex(system_prompt.as_bytes()) });
            }
            let name = options.name.as_deref().unwrap_or("pi-coding-agent");
            match verify_system_prompt(&system_prompt, name, options.expected_pi_documentation) {
                Err(error) => errors.push(error.to_string()),
                Ok(_) => match &options.output {
                    None => result = Value::String(response),
                    Some(output) => {
                        let context = OutputContext {
                            response,
                            system_prompt,
                            agent_dir: state.paths.agent_dir.clone(),
                            workspace: state.paths.workspace.clone(),
                            session_file: state.session_file.clone().unwrap_or_default(),
                            messages: state.snapshot.as_ref().map(|s| s.messages.clone()).unwrap_or_default(),
                            env: child_env,
                        };
                        match output(context).await {
                            Ok(value) => result = value,
                            Err(error) => errors.push(format!("{error:#}")),
                        }
                    }
                },
            }
        }
        Err(error) => errors.push(format!("{error:#}")),
    }
    match state.session_file.as_ref().map(std::fs::read_to_string) {
        Some(Ok(text)) => {
            if let Some(run) = &mut run {
                run.artifacts.insert(PI_SESSION_SNAPSHOT_ARTIFACT.into(), text);
            }
        }
        Some(Err(error)) => errors.push(error.to_string()),
        None if errors.is_empty() => errors.push("Pi eval produced no session file.".into()),
        None => {}
    }
    if let Err(error) = std::fs::remove_dir_all(&state.paths.root) {
        errors.push(error.to_string());
    }
    let total_ms = started.elapsed().as_secs_f64() * 1000.0;
    if !errors.is_empty() {
        let partial = run.map(|mut run| {
            run.total_ms = total_ms;
            run.errors = errors.clone();
            Box::new(run)
        });
        return Err(HarnessFailure { error: errors.join("; "), partial });
    }
    let mut run = run.unwrap_or_default();
    run.output = result;
    run.total_ms = total_ms;
    Ok(run)
}

fn diagnostics(snapshot: &SessionSnapshot, agent_dir: &Path) -> HarnessRun {
    let mut artifacts = BTreeMap::new();
    if let Some(id) = &snapshot.session_id {
        artifacts.insert("runId".to_string(), id.clone());
    }
    HarnessRun { events: snapshot.events(), usage: snapshot.usage(agent_dir), artifacts, ..HarnessRun::default() }
}
