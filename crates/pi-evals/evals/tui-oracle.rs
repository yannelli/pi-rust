//! Renders the interactive footer of a configured installation on a virtual terminal, with
//! a local faux model whose context window is 272k and fixed context-usage fixtures.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use pi_ai::providers::faux::FauxProvider;
use pi_ai::{Message, ThinkingLevel, register_api_provider};
use pi_coding_agent::agent_session::services::ResourceOptions;
use pi_coding_agent::app::{AppOptions, create_app_runtime};
use pi_coding_agent::interactive::{InteractiveMode, InteractiveOptions};
use pi_coding_agent::keybindings::KeybindingsManager;
use pi_coding_agent::session::SessionManager;
use pi_coding_agent::settings::schema::TuiMode;
use pi_tui::testing::VirtualTerminal;
use serde_json::{Value, json};

pub const CONTEXT_WINDOW: u64 = 272_000;
pub const BUILT_IN_CONTEXT_PATTERN: &str = r"\d+(?:\.\d+)?%/272k(?: \(auto\))?";
pub const CONTEXT_FIXTURES: [(f64, &str); 3] =
    [(42.2, "████░░░░░░ 42%"), (65.0, "███████░░░ 65%"), (120.0, "██████████ 100%")];

const PROVIDER: &str = "eval-oracle";
const MODEL: &str = "oracle";
static NEXT_API: AtomicU64 = AtomicU64::new(1);

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn assistant(api: &str, input: u64, output: u64, cache_read: u64, cost: f64) -> anyhow::Result<Message> {
    let total = input + output + cache_read;
    Ok(serde_json::from_value(json!({
        "role": "assistant", "content": [{ "type": "text", "text": "Context usage fixture" }],
        "api": api, "provider": PROVIDER, "model": MODEL,
        "usage": {
            "input": input, "output": output, "cacheRead": cache_read, "cacheWrite": 0, "totalTokens": total,
            "cost": { "input": cost, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": cost },
        },
        "stopReason": "stop", "timestamp": 0,
    }))?)
}

/// Point the copied installation at a faux model with a 272k context window.
fn configure_oracle_model(agent_dir: &Path, api: &str) -> anyhow::Result<()> {
    let path = agent_dir.join("models.json");
    let mut models = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    if !models["providers"].is_object() {
        models["providers"] = json!({});
    }
    models["providers"][PROVIDER] = json!({
        "baseUrl": "http://127.0.0.1:9", "api": api, "apiKey": "oracle",
        "models": [{ "id": MODEL, "contextWindow": CONTEXT_WINDOW, "maxTokens": 4000, "reasoning": true }],
    });
    std::fs::write(path, serde_json::to_string_pretty(&models)?)?;
    Ok(())
}

/// The visible screen and context usage with the latest response at `percent` of the window.
pub async fn render_footer(
    agent_dir: &Path,
    workspace: &Path,
    percent: f64,
    extensions: bool,
) -> anyhow::Result<(String, Value)> {
    let oracle = tempfile::Builder::new().prefix("pi-eval-tui-").tempdir()?;
    let oracle_agent_dir = oracle.path().join("agent");
    copy_tree(agent_dir, &oracle_agent_dir)?;
    let api = format!("eval-oracle-faux-{}", NEXT_API.fetch_add(1, Ordering::Relaxed));
    configure_oracle_model(&oracle_agent_dir, &api)?;
    register_api_provider(api.clone(), FauxProvider::new(Vec::new()));
    let mut manager = SessionManager::in_memory(workspace);
    manager.append_message(&assistant(&api, 106_000, 5_800, 606_000, 1.008)?)?;
    let tokens = (CONTEXT_WINDOW as f64 * percent / 100.0).round() as u64;
    manager.append_message(&assistant(&api, tokens, 0, 0, 0.0)?)?;
    let options = AppOptions {
        agent_dir: oracle_agent_dir.clone(),
        provider: Some(PROVIDER.into()),
        model: Some(MODEL.into()),
        thinking: Some(ThinkingLevel::High),
        resources: ResourceOptions { no_extensions: !extensions, ..ResourceOptions::default() },
        ..AppOptions::default()
    };
    let runtime = create_app_runtime(options, manager).await?;
    KeybindingsManager::create(&oracle_agent_dir).install();
    let terminal = VirtualTerminal::shared(100, 30);
    let interactive = InteractiveOptions {
        terminal: Some(terminal.clone()),
        agent_dir: Some(oracle_agent_dir),
        tui_mode: Some(TuiMode::Regular),
        skip_update_checks: true,
        ..InteractiveOptions::default()
    };
    let mode = InteractiveMode::new(Arc::new(runtime), interactive);
    let running = mode.clone();
    let task = tokio::task::spawn_local(async move {
        let _ = running.run().await;
    });
    let started = Instant::now();
    while !mode.is_ready() {
        if started.elapsed() > Duration::from_secs(20) {
            task.abort();
            anyhow::bail!("The interactive footer oracle did not start.");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let screen = terminal.borrow().viewport().join("\n");
    let usage = mode.current_session().context_usage().map_or(
        Value::Null,
        |usage| json!({ "tokens": usage.tokens, "contextWindow": usage.context_window, "percent": usage.percent }),
    );
    task.abort();
    Ok((screen, usage))
}

pub fn lines(output: &str) -> Vec<&str> {
    output.split(['\r', '\n']).filter(|line| !line.trim().is_empty()).collect()
}

/// The footer output the context-footer judge scores. The built-in status line comes
/// from a render with extensions disabled.
pub async fn inspect_context_footer(agent_dir: &Path, workspace: &Path) -> anyhow::Result<Value> {
    let pattern = regex::Regex::new(BUILT_IN_CONTEXT_PATTERN)?;
    let (reference, _) = render_footer(agent_dir, workspace, CONTEXT_FIXTURES[0].0, false).await?;
    let built_in = lines(&reference).into_iter().find(|line| pattern.is_match(line)).map(str::to_string);
    let mut observations = Vec::new();
    let mut extension_errors = Vec::new();
    for (percent, _) in CONTEXT_FIXTURES {
        let (screen, usage) = render_footer(agent_dir, workspace, percent, true).await?;
        observations.push(json!({ "percent": percent, "renderedOutput": screen, "contextUsage": usage }));
    }
    let runtime = super::load_agent_runtime(agent_dir, workspace).await?;
    if let Some(runner) = runtime.session().extension_runner() {
        extension_errors.extend(runner.load_errors().into_iter().map(|error| error.error));
    }
    Ok(json!({ "builtInStatusLine": built_in, "observations": observations, "extensionErrors": extension_errors }))
}
