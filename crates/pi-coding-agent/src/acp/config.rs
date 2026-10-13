//! Model and thinking level as ACP config options and modes (upstream `getSessionConfiguration`,
//! `buildConfigOptions`, `getModelState`, and `setSessionModel` in `agent.ts`).

use agent_client_protocol_schema::{
    ConfigOptionUpdate, CurrentModeUpdate, Error, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelectOption, SessionMode, SessionModeState, SessionNotification, SessionUpdate,
};
use serde::Serialize;
use serde_json::Value;

use super::backend::{AcpClient, PiBackend};
use super::errors::invalid_params;

pub const MODEL_CONFIG_ID: &str = "model";
pub const THOUGHT_LEVEL_CONFIG_ID: &str = "thought_level";

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

/// pi's thinking levels as modes; fails when the current level is not among them.
async fn thinking_state(backend: &dyn PiBackend, state: &Value) -> anyhow::Result<SessionModeState> {
    let available = backend.get_available_thinking_levels().await?;
    let current = state.get("thinkingLevel").and_then(Value::as_str).unwrap_or_default();
    if current.is_empty() || !available.iter().any(|level| level == current) {
        anyhow::bail!("pi returned a thinking level absent from available levels");
    }
    let modes = available.iter().map(|id| SessionMode::new(id.clone(), format!("Thinking: {id}"))).collect();
    Ok(SessionModeState::new(current.to_string(), modes))
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
            Some(AdvertisedModel {
                model_id: format!("{provider}/{id}"),
                name: format!("{provider}/{name}"),
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

/// The thinking selector, preceded by the model selector when models are known.
pub fn build_config_options(models: Option<&ModelState>, modes: &SessionModeState) -> Vec<SessionConfigOption> {
    let thinking: Vec<SessionConfigSelectOption> = modes
        .available_modes
        .iter()
        .map(|mode| SessionConfigSelectOption::new(mode.id.to_string(), mode.name.clone()))
        .collect();
    let mut options = vec![
        SessionConfigOption::select(THOUGHT_LEVEL_CONFIG_ID, "Thinking", modes.current_mode_id.to_string(), thinking)
            .category(SessionConfigOptionCategory::ThoughtLevel)
            .description("Set the reasoning effort for this session"),
    ];
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
    let modes = thinking_state(backend, &state).await?;
    let config_options = build_config_options(model_state.as_ref(), &modes);
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
