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

/// A model of an agent. The window is the `size` of the agent's last `usage_update` for it, or 0
/// (unknown) before one; ACP has no output limit, so `maxTokens` is 0 too.
fn model(id: &str, name: &str, state: &HostState, provider: &str) -> Value {
    let reasoning = state.thinking.get(provider).copied().unwrap_or(false);
    let window = state.context.get(provider).and_then(|windows| windows.get(id)).copied().unwrap_or(0);
    let input = match state.images.get(provider) {
        Some(false) => json!(["text"]),
        _ => json!(["text", "image"]),
    };
    json!({"id": id, "name": name, "reasoning": reasoning, "input": input, "contextWindow": window, "maxTokens": 0})
}

fn config(name: &str, id: &str, state: &HostState, provider: &str) -> Value {
    let mut models = vec![model(DEFAULT_MODEL, &format!("{name} (agent default)"), state, provider)];
    let advertised = state.models.get(provider).into_iter().flatten();
    let others = advertised.filter(|choice| choice.id != DEFAULT_MODEL);
    models.extend(others.map(|choice| model(&choice.id, &choice.name, state, provider)));
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
