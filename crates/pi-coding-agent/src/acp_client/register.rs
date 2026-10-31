//! The enabled `acpAgents` entries as providers with the API `acp`: one `default` model plus the
//! models each agent advertised, and the launch commands that pi-ai starts.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};

use pi_ai::providers::acp::{self, AgentLaunch, DEFAULT_MODEL};
use serde_json::{Value, json};

use super::host::{self, HostState};
use super::{AcpAgentEntry, provider_id, store};
use crate::agent_session::ModelRuntime;
use crate::settings::SettingsManager;

/// Agents do not report a window up front; a large one keeps pi from compacting their sessions.
const CONTEXT_WINDOW: u64 = 1_000_000;
const MAX_TOKENS: u64 = 64_000;

static REGISTERED: LazyLock<Mutex<BTreeSet<String>>> = LazyLock::new(Mutex::default);

pub fn launch(entry: &AcpAgentEntry) -> AgentLaunch {
    AgentLaunch {
        provider: provider_id(&entry.id),
        name: entry.name.clone(),
        command: entry.command.clone(),
        args: entry.args.clone(),
        env: entry.env.clone(),
    }
}

fn model(id: &str, name: &str, reasoning: bool) -> Value {
    json!({"id": id, "name": name, "reasoning": reasoning, "input": ["text", "image"],
        "contextWindow": CONTEXT_WINDOW, "maxTokens": MAX_TOKENS})
}

fn config(name: &str, id: &str, state: &HostState, provider: &str) -> Value {
    let reasoning = state.thinking.get(provider).copied().unwrap_or(false);
    let mut models = vec![model(DEFAULT_MODEL, &format!("{name} (agent default)"), reasoning)];
    let advertised = state.models.get(provider).into_iter().flatten();
    models.extend(advertised.filter(|choice| choice.id != DEFAULT_MODEL).map(|c| model(&c.id, &c.name, reasoning)));
    json!({"name": name, "api": acp::API, "baseUrl": format!("acp://{id}"), "apiKey": "acp", "models": models})
}

/// Registers the enabled agents of `settings` and removes the providers of the others.
pub fn sync(runtime: &Arc<ModelRuntime>, settings: &SettingsManager, agent_dir: &Path) {
    host::install(agent_dir, runtime);
    let state = host::read_state(agent_dir);
    let entries: Vec<AcpAgentEntry> =
        store::list(settings).into_iter().map(|(entry, _)| entry).filter(|entry| entry.enabled).collect();
    acp::set_agents(entries.iter().map(launch).collect());
    let mut registered = REGISTERED.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let current: BTreeSet<String> = entries.iter().map(|entry| provider_id(&entry.id)).collect();
    for stale in registered.difference(&current) {
        runtime.unregister_extension_provider(stale);
    }
    for entry in &entries {
        let provider = provider_id(&entry.id);
        let _ = runtime.register_extension_provider(&provider, config(&entry.name, &entry.id, &state, &provider));
    }
    *registered = current;
}

/// Re-registers `provider` with the models in the host state, after its agent advertised new ones.
pub(super) fn refresh_models(runtime: &Arc<ModelRuntime>, agent_dir: &Path, provider: &str) {
    let Some(agent) = acp::agent(provider) else { return };
    let state = host::read_state(agent_dir);
    let _ = runtime.register_extension_provider(provider, config(&agent.name, provider, &state, provider));
}
