//! The enabled `acpAgents` entries as providers with the API `acp`: one `default` model plus the
//! models each agent advertised, and the launch commands that pi-ai starts.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};

use pi_ai::providers::acp::{self, AgentLaunch, Choice, DEFAULT_MODEL};
use pi_ai::types::ThinkingLevel;
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

/// Each pi thinking level to the agent value whose ID or name is that level, ignoring case, and
/// `None` for the levels the agent does not offer; `None` when it offers none of them.
pub(super) fn thinking_level_map(choices: &[Choice]) -> Option<BTreeMap<String, Option<String>>> {
    let value = |level: ThinkingLevel| {
        let named = |text: &str| text.eq_ignore_ascii_case(level.as_str());
        choices.iter().find(|choice| named(&choice.id) || named(&choice.name)).map(|choice| choice.id.clone())
    };
    let map: BTreeMap<String, Option<String>> =
        ThinkingLevel::ALL.into_iter().map(|level| (level.as_str().to_string(), value(level))).collect();
    map.values().any(Option::is_some).then_some(map)
}

/// A model of an agent. The window is the `size` of the agent's last `usage_update` for it, or 0
/// (unknown) before one; ACP has no output limit, so `maxTokens` is 0 too.
fn model(id: &str, name: &str, state: &HostState, provider: &str) -> Value {
    let levels = state.thought_levels.get(provider).map(Vec::as_slice).and_then(thinking_level_map);
    let advertised = state.thought_levels.contains_key(provider);
    let reasoning = match advertised {
        true => levels.is_some(),
        false => state.thinking.get(provider).copied().unwrap_or(false),
    };
    let window = state.context.get(provider).and_then(|windows| windows.get(id)).copied().unwrap_or(0);
    let input = match state.images.get(provider) {
        Some(false) => json!(["text"]),
        _ => json!(["text", "image"]),
    };
    let mut model = json!({"id": id, "name": name, "reasoning": reasoning, "input": input,
        "contextWindow": window, "maxTokens": 0});
    if let Some(levels) = levels {
        model["thinkingLevelMap"] = json!(levels);
    }
    model
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
