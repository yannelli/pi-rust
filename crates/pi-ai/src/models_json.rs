//! `models.json` parsing and its application to the catalog (upstream `core/model-config.ts`
//! and `core/provider-composer.ts`): custom providers and models, provider `baseUrl` and
//! `compat` for built-in models, and `modelOverrides`.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::auth::ProviderAuthConfig;
use crate::types::{Model, ModelCost};

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ModelsFile {
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProviderConfig {
    pub name: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub api: Option<String>,
    pub oauth: Option<String>,
    pub headers: Option<BTreeMap<String, String>>,
    pub compat: Option<Map<String, Value>>,
    pub auth_header: Option<bool>,
    pub models: Option<Vec<ModelDefinition>>,
    pub model_overrides: Option<BTreeMap<String, Map<String, Value>>>,
}

/// One `models` entry. `type` and `output` describe image and classifier models.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ModelDefinition {
    pub id: String,
    pub name: Option<String>,
    pub api: Option<String>,
    pub base_url: Option<String>,
    #[serde(rename = "type")]
    pub model_type: Option<String>,
    pub reasoning: Option<bool>,
    pub thinking_level_map: Option<BTreeMap<String, Option<String>>>,
    pub input: Option<Vec<String>>,
    pub output: Option<Vec<String>>,
    pub input_limits: Option<Value>,
    pub cost: Option<ModelCost>,
    pub prompt_cache: Option<BTreeMap<String, f64>>,
    pub context_window: Option<f64>,
    pub max_tokens: Option<f64>,
    pub sampling_params: Option<Map<String, Value>>,
    pub sampling_params_by_thinking_level: Option<BTreeMap<String, Map<String, Value>>>,
    pub headers: Option<BTreeMap<String, String>>,
    pub compat: Option<Map<String, Value>>,
}

/// Remove `//` and `/* */` comments outside strings.
pub(crate) fn strip_json_comments(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(character) = chars.next() {
        if in_string {
            output.push(character);
            match character {
                '\\' => output.extend(chars.next()),
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match (character, chars.peek()) {
            ('"', _) => {
                in_string = true;
                output.push(character);
            }
            ('/', Some('/')) => {
                for next in chars.by_ref() {
                    if next == '\n' {
                        output.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut previous = ' ';
                for next in chars.by_ref() {
                    if previous == '*' && next == '/' {
                        break;
                    }
                    previous = next;
                }
            }
            _ => output.push(character),
        }
    }
    output
}

pub(crate) fn parse_models_file(text: &str) -> Result<ModelsFile, String> {
    let stripped = strip_json_comments(text.trim_start_matches('\u{feff}'));
    serde_json::from_str(&stripped).map_err(|error| error.to_string())
}

use super::is_chat_model as is_chat;

const NESTED_COMPAT_KEYS: [&str; 4] =
    ["openRouterRouting", "vercelGatewayRouting", "chatTemplateKwargs", "chatTemplateArgs"];

fn merge_compat(base: Option<&Value>, patch: Option<&Map<String, Value>>) -> Option<Value> {
    let Some(patch) = patch else { return base.cloned() };
    let mut merged = base.and_then(Value::as_object).cloned().unwrap_or_default();
    for (key, value) in patch {
        let nested = NESTED_COMPAT_KEYS.contains(&key.as_str());
        match (merged.get_mut(key), value) {
            (Some(Value::Object(existing)), Value::Object(update)) if nested => {
                existing.extend(update.iter().map(|(k, v)| (k.clone(), v.clone())));
            }
            _ => {
                merged.insert(key.clone(), value.clone());
            }
        }
    }
    Some(Value::Object(merged))
}

fn shallow_merge(target: &mut Map<String, Value>, key: &str, patch: &Value) {
    match (target.get_mut(key), patch) {
        (Some(Value::Object(existing)), Value::Object(update)) => {
            existing.extend(update.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        _ => {
            target.insert(key.to_string(), patch.clone());
        }
    }
}

/// Apply one `modelOverrides` entry the way upstream `applyModelOverride` merges fields.
pub(crate) fn apply_override(model: &Model, patch: &Map<String, Value>) -> Result<Model, String> {
    let Value::Object(mut target) = serde_json::to_value(model).map_err(|error| error.to_string())? else {
        return Err("model did not serialize to an object".into());
    };
    for (key, value) in patch {
        match key.as_str() {
            "name" | "reasoning" | "input" | "contextWindow" | "maxTokens" => {
                target.insert(key.clone(), value.clone());
            }
            "thinkingLevelMap" | "promptCache" | "samplingParams" | "cost" => shallow_merge(&mut target, key, value),
            "samplingParamsByThinkingLevel" => {
                let levels = target.entry(key.clone()).or_insert_with(|| Value::Object(Map::new()));
                if let (Value::Object(levels), Value::Object(update)) = (levels, value) {
                    for (level, params) in update {
                        shallow_merge(levels, level, params);
                    }
                }
            }
            "inputLimits" => {
                let limits = target.entry(key.clone()).or_insert_with(|| Value::Object(Map::new()));
                if let (Value::Object(limits), Value::Object(update)) = (limits, value) {
                    for (field, field_value) in update {
                        if field != "images" {
                            limits.insert(field.clone(), field_value.clone());
                            continue;
                        }
                        let images = limits.entry("images").or_insert_with(|| Value::Object(Map::new()));
                        if let (Value::Object(images), Value::Object(image_update)) = (images, field_value) {
                            for (image_key, image_value) in image_update {
                                shallow_merge(images, image_key, image_value);
                            }
                        }
                    }
                }
            }
            "compat" => {
                if let Some(merged) = merge_compat(target.get("compat"), value.as_object()) {
                    target.insert(key.clone(), merged);
                }
            }
            _ => {}
        }
    }
    serde_json::from_value(Value::Object(target)).map_err(|error| error.to_string())
}

fn positive(value: Option<f64>, field: &str, provider: &str, id: &str) -> Result<Option<u64>, String> {
    match value {
        Some(value) if value.is_nan() || value <= 0.0 => {
            Err(format!("Provider {provider}, model {id}: invalid {field}"))
        }
        value => Ok(value.map(|value| value as u64)),
    }
}

fn model_from_definition(
    provider: &str,
    definition: &ModelDefinition,
    config: &ProviderConfig,
    defaults: Option<&Model>,
) -> Result<Model, String> {
    let id = &definition.id;
    let api = definition.api.clone().or_else(|| config.api.clone()).or_else(|| defaults.map(|model| model.api.clone()));
    let api = api.ok_or_else(|| {
        format!("Provider {provider}, model {id}: no \"api\" specified. Set at provider or model level.")
    })?;
    let base_url = definition
        .base_url
        .clone()
        .or_else(|| config.base_url.clone())
        .or_else(|| defaults.map(|m| m.base_url.clone()));
    let base_url =
        base_url.ok_or_else(|| format!("Provider {provider}: \"baseUrl\" is required when defining custom models."))?;
    let input_limits = match &definition.input_limits {
        Some(value) => Some(
            serde_json::from_value(value.clone())
                .map_err(|error| format!("Provider {provider}, model {id}: invalid inputLimits: {error}"))?,
        ),
        None => None,
    };
    Ok(Model {
        id: id.clone(),
        name: definition.name.clone().unwrap_or_else(|| id.clone()),
        api,
        provider: provider.to_string(),
        base_url,
        model_type: definition.model_type.clone().filter(|kind| kind != "chat"),
        reasoning: definition.reasoning.unwrap_or(false),
        thinking_level_map: definition.thinking_level_map.clone(),
        prompt_cache: definition.prompt_cache.clone(),
        input_limits,
        output: definition.output.clone(),
        input: definition.input.clone().unwrap_or_else(|| vec!["text".into()]),
        cost: definition.cost.clone().unwrap_or_default(),
        context_window: positive(definition.context_window, "contextWindow", provider, id)?.unwrap_or(128_000),
        max_tokens: positive(definition.max_tokens, "maxTokens", provider, id)?.unwrap_or(16_384),
        sampling_params: definition.sampling_params.clone(),
        sampling_params_by_thinking_level: definition.sampling_params_by_thinking_level.clone(),
        headers: BTreeMap::new(),
        compat: merge_compat(merge_compat(None, config.compat.as_ref()).as_ref(), definition.compat.as_ref()),
    })
}

fn find_defaults<'a>(models: &'a [Model], id: &str, api: Option<&str>) -> Option<&'a Model> {
    let chat = || models.iter().filter(|model| is_chat(model));
    chat()
        .find(|model| model.id == id)
        .or_else(|| api.and_then(|api| chat().find(|model| model.api == api)))
        .or_else(|| chat().find(|model| model.api == "openai-completions"))
        .or_else(|| chat().next())
}

/// Apply one provider's configuration to its models (built-in plus custom).
pub(crate) fn apply_provider(provider: &str, base: Vec<Model>, config: &ProviderConfig) -> Result<Vec<Model>, String> {
    if config.oauth.is_some() && config.base_url.is_none() {
        return Err(format!("Provider {provider}: \"baseUrl\" is required when \"oauth\" is set."));
    }
    let has_overrides = config.model_overrides.as_ref().is_some_and(|overrides| !overrides.is_empty());
    let has_models = config.models.as_ref().is_some_and(|models| !models.is_empty());
    if !has_models
        && config.base_url.is_none()
        && config.headers.is_none()
        && config.compat.is_none()
        && !has_overrides
        && config.api_key.is_none()
        && config.oauth.is_none()
        && config.auth_header.is_none()
    {
        return Err(format!(
            "Provider {provider}: must specify \"baseUrl\", \"headers\", \"compat\", \"modelOverrides\", or \"models\"."
        ));
    }
    let mut models: Vec<Model> = base
        .into_iter()
        .map(|mut model| {
            if config.oauth.as_deref() != Some("radius")
                && let Some(base_url) = &config.base_url
            {
                model.base_url = base_url.clone();
            }
            if is_chat(&model) {
                model.compat = merge_compat(model.compat.as_ref(), config.compat.as_ref());
            }
            model
        })
        .collect();
    for definition in config.models.iter().flatten() {
        let defaults = find_defaults(&models, &definition.id, definition.api.as_deref().or(config.api.as_deref()));
        let model = model_from_definition(provider, definition, config, defaults)?;
        let kind = model.model_type.clone();
        match models.iter_mut().find(|existing| existing.id == model.id && existing.model_type == kind) {
            Some(existing) => *existing = model,
            None => models.push(model),
        }
    }
    for (id, patch) in config.model_overrides.iter().flatten() {
        for model in models.iter_mut().filter(|model| &model.id == id && is_chat(model)) {
            *model =
                apply_override(model, patch).map_err(|error| format!("Provider {provider}, model {id}: {error}"))?;
        }
    }
    Ok(models)
}

/// The auth-related parts of a provider config.
pub(crate) fn auth_config(config: &ProviderConfig) -> ProviderAuthConfig {
    let mut model_headers: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for (id, patch) in config.model_overrides.iter().flatten() {
        if let Some(Value::Object(headers)) = patch.get("headers") {
            let entry = model_headers.entry(id.clone()).or_default();
            entry.extend(headers.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))));
        }
    }
    for definition in config.models.iter().flatten() {
        if let Some(headers) = &definition.headers {
            model_headers.entry(definition.id.clone()).or_default().extend(headers.clone());
        }
    }
    ProviderAuthConfig {
        api_key: config.api_key.clone(),
        headers: config.headers.clone().unwrap_or_default(),
        auth_header: config.auth_header.unwrap_or(false),
        model_headers,
    }
}
