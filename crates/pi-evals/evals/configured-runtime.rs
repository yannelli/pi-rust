//! Oracles that report the models and providers a configured pi installation exposes.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use pi_ai::auth::AuthOverrides;
use pi_ai::{Context, Message, Model, ModelRegistry, StreamOptions, UserContent, UserMessage};
use pi_coding_agent::agent_session::model_runtime::{ModelRuntime, ModelRuntimeOptions};
use serde_json::{Value, json};

use crate::session::content_text;

/// A probe of one configured provider model.
#[derive(Clone)]
pub struct ProviderScenario {
    pub provider_id: String,
    pub model_id: String,
    pub prompt: String,
    pub env: BTreeMap<String, String>,
    pub max_tokens: u64,
    pub valid_request_received: Arc<dyn Fn() -> bool + Send + Sync>,
}

/// The model fields the oracles compare.
pub fn model_fields(model: &Model) -> Value {
    let full = serde_json::to_value(model).unwrap_or_default();
    let cost = &full["cost"];
    json!({
        "id": full["id"], "name": full["name"], "provider": full["provider"], "reasoning": full["reasoning"],
        "input": full["input"],
        "cost": {
            "input": cost["input"], "output": cost["output"],
            "cacheRead": cost["cacheRead"], "cacheWrite": cost["cacheWrite"],
        },
        "contextWindow": full["contextWindow"], "maxTokens": full["maxTokens"],
    })
}

/// The runtime for `models.json` and `auth.json` in `agent_dir`.
pub async fn load_configured_model_runtime(agent_dir: &Path) -> ModelRuntime {
    ModelRuntime::create(ModelRuntimeOptions::for_agent_dir(agent_dir)).await
}

fn error(message: impl Into<String>) -> Value {
    json!({ "result": { "error": message.into() } })
}

async fn complete(runtime: &ModelRuntime, model: &Model, scenario: &ProviderScenario) -> Result<Value, String> {
    let overrides = AuthOverrides { env: Some(scenario.env.clone()), ..AuthOverrides::default() };
    let resolved = runtime
        .resolver()
        .resolve_for_model(&model.provider, &model.id, &overrides)
        .await
        .map_err(|error| error.to_string())?;
    let mut model = model.clone();
    let mut options =
        StreamOptions { max_tokens: Some(scenario.max_tokens), max_retries: Some(0), ..StreamOptions::default() };
    options.env = scenario.env.clone();
    if let Some(resolved) = resolved {
        if let Some(base_url) = resolved.auth.base_url {
            model.base_url = base_url;
        }
        options.api_key = resolved.auth.api_key;
        options.headers = resolved.auth.headers;
        options.env.extend(resolved.env.unwrap_or_default());
    }
    let user = UserMessage { content: UserContent::Text(scenario.prompt.clone()), timestamp: 0 };
    let context = Context { messages: vec![Message::User(user)], ..Context::default() };
    let response = pi_ai::complete(&model, &context, options).await;
    Ok(json!({
        "text": content_text(&response.content),
        "stopReason": serde_json::to_value(response.stop_reason).unwrap_or_default(),
        "inputTokens": response.usage.input,
        "outputTokens": response.usage.output,
    }))
}

/// Load the configured model and send it the probe prompt.
pub async fn inspect_provider(runtime: &ModelRuntime, scenario: &ProviderScenario) -> Value {
    if let Some(configuration_error) = runtime.load_error() {
        return error(configuration_error);
    }
    let Some(model) = runtime.get_model(&scenario.provider_id, &scenario.model_id) else {
        return error(format!("Model {}/{} is unavailable after reload.", scenario.provider_id, scenario.model_id));
    };
    match complete(runtime, &model, scenario).await {
        Err(message) => error(message),
        Ok(response) => json!({
            "result": {
                "validRequestReceived": (scenario.valid_request_received)(),
                "model": model_fields(&model),
                "response": response,
            },
        }),
    }
}

/// The added model, and whether every built-in model of its provider remains.
pub async fn inspect_added_model(runtime: &ModelRuntime, provider_id: &str, model_id: &str) -> Value {
    let existing: Vec<String> =
        ModelRegistry::builtin().provider_models(provider_id).into_iter().map(|model| model.id).collect();
    if existing.is_empty() {
        return error(format!("Built-in provider {provider_id} has no models."));
    }
    if let Some(configuration_error) = runtime.load_error() {
        return error(configuration_error);
    }
    let Some(model) = runtime.get_model(provider_id, model_id) else {
        return error(format!("Model {provider_id}/{model_id} is unavailable after reload."));
    };
    let preserved = existing.iter().all(|id| runtime.get_model(provider_id, id).is_some());
    json!({ "result": { "model": model_fields(&model), "existingModelsPreserved": preserved } })
}
