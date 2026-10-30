//! ACP model, thinking, and permission selectors. Legacy modes represent permissions only.

use agent_client_protocol_schema::{
    ConfigOptionUpdate, CurrentModeUpdate, Error, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelectOption, SessionModeState, SessionNotification, SessionUpdate,
};
use serde::Serialize;
use serde_json::Value;

use super::backend::{AcpClient, PiBackend};
use super::errors::invalid_params;

pub const MODEL_CONFIG_ID: &str = "model";
pub const MODE_CONFIG_ID: &str = "mode";
pub const THOUGHT_LEVEL_CONFIG_ID: &str = "thought_level";
/// pi-rust extension: the "Speed" selector for models with a fast mode.
pub const FAST_MODE_CONFIG_ID: &str = "fast_mode";
pub const FAST_MODE_STANDARD: &str = "standard";
pub const FAST_MODE_FAST: &str = "fast";

/// A model as advertised: `provider/id`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdvertisedModel {
    pub model_id: String,
    pub name: String,
    pub description: Option<String>,
}

/// The legacy `models` field of `session/new` and `session/load` responses.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelState {
    pub available_models: Vec<AdvertisedModel>,
    pub current_model_id: String,
}

/// The config options plus the legacy `models` and `modes` views.
pub struct SessionConfiguration {
    pub config_options: Vec<SessionConfigOption>,
    pub models: Option<ModelState>,
    pub modes: SessionModeState,
}

fn thinking_name(level: &str) -> &str {
    match level {
        "off" => "Off",
        "minimal" => "Minimal",
        "low" => "Low",
        "medium" => "Medium",
        "high" => "High",
        "xhigh" => "Extra High",
        "max" => "Max",
        other => other,
    }
}

fn provider_name(provider: &str) -> &str {
    match provider {
        "xai" => "xAI",
        "openai" => "OpenAI",
        "openai-codex" => "OpenAI Codex",
        "anthropic" => "Anthropic",
        "google" => "Google",
        "google-vertex" => "Google Vertex AI",
        "google-antigravity" => "Google Antigravity",
        "github-copilot" => "GitHub Copilot",
        "amazon-bedrock" => "Amazon Bedrock",
        "openrouter" => "OpenRouter",
        "azure-openai-responses" => "Azure OpenAI",
        "mistral" => "Mistral",
        "deepseek" => "DeepSeek",
        "groq" => "Groq",
        "cerebras" => "Cerebras",
        "kimi-coding" => "Kimi Coding",
        "cursor" => "Cursor",
        "meta" => "Meta",
        "radius" => "Radius",
        other => other,
    }
}

/// Fail when pi reports a current level absent from the model's available levels.
async fn thinking_option(backend: &dyn PiBackend, state: &Value) -> anyhow::Result<SessionConfigOption> {
    let available = backend.get_available_thinking_levels().await?;
    let current = state.get("thinkingLevel").and_then(Value::as_str).unwrap_or_default();
    if current.is_empty() || !available.iter().any(|level| level == current) {
        anyhow::bail!("pi returned a thinking level absent from available levels");
    }
    let choices: Vec<_> =
        available.iter().map(|id| SessionConfigSelectOption::new(id.clone(), thinking_name(id))).collect();
    Ok(SessionConfigOption::select(THOUGHT_LEVEL_CONFIG_ID, "Thinking", current.to_string(), choices)
        .category(SessionConfigOptionCategory::ThoughtLevel)
        .description("Set the reasoning effort for this session"))
}

/// The "Speed" selector; `None` when the current model has no fast mode.
fn fast_mode_option(state: &Value) -> Option<SessionConfigOption> {
    if state.get("fastModeAvailable").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let fast = state.get("fastMode").and_then(Value::as_bool).unwrap_or(false);
    let current = if fast { FAST_MODE_FAST } else { FAST_MODE_STANDARD };
    let choices = vec![
        SessionConfigSelectOption::new(FAST_MODE_STANDARD, "Standard"),
        SessionConfigSelectOption::new(FAST_MODE_FAST, "Fast"),
    ];
    let option = SessionConfigOption::select(FAST_MODE_CONFIG_ID, "Speed", current, choices)
        .category(SessionConfigOptionCategory::Other("_speed".into()))
        .description("Run the model in its fast mode");
    Some(option)
}

/// Available models and the current one; `None` when pi reports neither.
fn model_state(models: Option<&Value>, state: Option<&Value>) -> Option<ModelState> {
    let field = |value: &Value, key: &str| match &value[key] {
        Value::String(text) => text.trim().to_string(),
        Value::Null => String::new(),
        other => other.to_string().trim().to_string(),
    };
    let available: Vec<AdvertisedModel> = models
        .and_then(|data| data["models"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|model| {
            let (provider, id) = (field(model, "provider"), field(model, "id"));
            if provider.is_empty() || id.is_empty() {
                return None;
            }
            let name = model["name"].as_str().map(String::from).unwrap_or_else(|| id.clone());
            let display_provider = model["providerName"]
                .as_str()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| provider_name(&provider));
            // ACP has no standard icon field; label existing fast variants without inventing support.
            let fast = id.split('-').any(|part| part == "fast");
            Some(AdvertisedModel {
                model_id: format!("{provider}/{id}"),
                name: format!("{display_provider}: {name}{}", if fast { " ⚡" } else { "" }),
                description: None,
            })
        })
        .collect();
    let current = state.map(|state| &state["model"]).filter(|model| model.is_object()).and_then(|model| {
        let (provider, id) = (field(model, "provider"), field(model, "id"));
        (!provider.is_empty() && !id.is_empty()).then(|| format!("{provider}/{id}"))
    });
    if available.is_empty() && current.is_none() {
        return None;
    }
    let current_model_id =
        current.or_else(|| available.first().map(|model| model.model_id.clone())).unwrap_or_else(|| "default".into());
    Some(ModelState { available_models: available, current_model_id })
}

/// Keep permissions separate from model-specific thinking levels.
fn build_config_options(
    models: Option<&ModelState>,
    modes: &SessionModeState,
    thinking: SessionConfigOption,
    fast_mode: Option<SessionConfigOption>,
) -> Vec<SessionConfigOption> {
    let choices: Vec<_> = modes
        .available_modes
        .iter()
        .map(|mode| {
            SessionConfigSelectOption::new(mode.id.to_string(), mode.name.clone()).description(mode.description.clone())
        })
        .collect();
    let mut options = vec![
        thinking,
        SessionConfigOption::select(MODE_CONFIG_ID, "Mode", modes.current_mode_id.to_string(), choices)
            .category(SessionConfigOptionCategory::Mode)
            .description("Control tool approval for this session"),
    ];
    options.extend(fast_mode);
    if let Some(models) = models.filter(|models| !models.available_models.is_empty()) {
        let choices: Vec<SessionConfigSelectOption> = models
            .available_models
            .iter()
            .map(|model| SessionConfigSelectOption::new(model.model_id.clone(), model.name.clone()))
            .collect();
        let option = SessionConfigOption::select(MODEL_CONFIG_ID, "Model", models.current_model_id.clone(), choices)
            .category(SessionConfigOptionCategory::Model)
            .description("Select the model for this session");
        options.insert(0, option);
    }
    options
}

/// Read pi's configuration; `state` and `models` reuse data fetched by the caller.
pub async fn session_configuration(
    backend: &dyn PiBackend,
    state: Option<Value>,
    models: Option<Value>,
) -> anyhow::Result<SessionConfiguration> {
    let state = match state {
        Some(state) => state,
        None => backend.get_state().await?,
    };
    let models = match models {
        Some(models) => Some(models),
        None => backend.get_available_models().await.ok(),
    };
    let model_state = model_state(models.as_ref(), Some(&state));
    let modes = backend.permission_mode().state();
    let thinking = thinking_option(backend, &state).await?;
    let config_options = build_config_options(model_state.as_ref(), &modes, thinking, fast_mode_option(&state));
    Ok(SessionConfiguration { config_options, models: model_state, modes })
}

/// Send `current_mode_update` and `config_option_update` with pi's current configuration.
pub async fn emit_config_options_update(
    client: &dyn AcpClient,
    session_id: &str,
    backend: &dyn PiBackend,
) -> anyhow::Result<Vec<SessionConfigOption>> {
    let configuration = session_configuration(backend, None, None).await?;
    let mode = CurrentModeUpdate::new(configuration.modes.current_mode_id.clone());
    client.session_update(SessionNotification::new(session_id.to_string(), SessionUpdate::CurrentModeUpdate(mode)));
    let options = ConfigOptionUpdate::new(configuration.config_options.clone());
    client.session_update(SessionNotification::new(session_id.to_string(), SessionUpdate::ConfigOptionUpdate(options)));
    Ok(configuration.config_options)
}

/// Switch to `provider/model`, or to a bare model id looked up in the available models.
pub async fn set_session_model(backend: &dyn PiBackend, requested: &str) -> Result<(), Error> {
    let (mut provider, mut model_id) = match requested.split_once('/') {
        Some((provider, rest)) => (Some(provider.to_string()), rest.to_string()),
        None => (None, requested.to_string()),
    };
    if provider.as_deref().is_none_or(str::is_empty) {
        provider = None;
        let data = backend.get_available_models().await.map_err(|error| super::errors::internal_failure(&error))?;
        let found = data["models"].as_array().into_iter().flatten().find(|model| model["id"] == model_id.as_str());
        if let Some(found) = found {
            provider = found["provider"].as_str().map(String::from);
            model_id = found["id"].as_str().unwrap_or_default().to_string();
        }
    }
    let Some(provider) = provider.filter(|_| !model_id.is_empty()) else {
        return Err(invalid_params(format!("Unknown modelId: {requested}")));
    };
    backend.set_model(&provider, &model_id).await.map_err(|error| super::errors::internal_failure(&error))
}
