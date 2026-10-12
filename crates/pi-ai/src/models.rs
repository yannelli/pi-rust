//! Bundled model catalog and the `models.json` overlay used by the TypeScript pi.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::LazyLock;

use serde::Deserialize;
use serde_json::Value;

use crate::env::get_env_api_key;
use crate::types::{Model, ModelCost};

static BUILTIN: LazyLock<Vec<Model>> =
    LazyLock::new(|| serde_json::from_str(include_str!("../models.json")).expect("bundled models.json is valid"));

/// Models shipped with this crate.
pub fn builtin_models() -> &'static [Model] {
    &BUILTIN
}

#[derive(Debug, thiserror::Error)]
pub enum ModelsFileError {
    #[error("cannot read {path}: {source}")]
    Read { path: String, source: std::io::Error },
    #[error("invalid models file {path}: {source}")]
    Parse { path: String, source: serde_json::Error },
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelsFile {
    #[serde(default)]
    providers: BTreeMap<String, ProviderConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderConfig {
    base_url: Option<String>,
    api: Option<String>,
    api_key: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    models: Vec<ModelConfig>,
    #[serde(default)]
    model_overrides: BTreeMap<String, Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelConfig {
    id: String,
    name: Option<String>,
    api: Option<String>,
    base_url: Option<String>,
    reasoning: Option<bool>,
    input: Option<Vec<String>>,
    cost: Option<ModelCost>,
    context_window: Option<u64>,
    max_tokens: Option<u64>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    compat: Option<Value>,
}

/// Available models and per-provider API key sources.
#[derive(Clone, Debug, Default)]
pub struct ModelRegistry {
    models: Vec<Model>,
    api_keys: HashMap<String, String>,
}

impl ModelRegistry {
    /// The bundled catalog only.
    pub fn builtin() -> Self {
        Self { models: builtin_models().to_vec(), api_keys: HashMap::new() }
    }

    /// The bundled catalog plus `path` when it exists.
    pub fn load(path: &Path) -> Result<Self, ModelsFileError> {
        let mut registry = Self::builtin();
        if path.exists() {
            let display = path.display().to_string();
            let text = std::fs::read_to_string(path)
                .map_err(|source| ModelsFileError::Read { path: display.clone(), source })?;
            registry.apply_json(&text).map_err(|source| ModelsFileError::Parse { path: display, source })?;
        }
        Ok(registry)
    }

    /// Apply a `models.json` document: add or replace models and record key sources.
    pub fn apply_json(&mut self, text: &str) -> Result<(), serde_json::Error> {
        let file: ModelsFile = serde_json::from_str(text)?;
        for (provider, config) in file.providers {
            if let Some(api_key) = &config.api_key {
                self.api_keys.insert(provider.clone(), api_key.clone());
            }
            for (id, patch) in &config.model_overrides {
                if let Some(model) = self.models.iter_mut().find(|model| model.provider == provider && &model.id == id)
                {
                    let mut value = serde_json::to_value(&*model)?;
                    merge(&mut value, patch);
                    *model = serde_json::from_value(value)?;
                }
            }
            let template = self.models.iter().find(|model| model.provider == provider).cloned();
            for entry in config.models {
                let base = self.models.iter().find(|model| model.provider == provider && model.id == entry.id).cloned();
                let fallback = base.as_ref().or(template.as_ref());
                let api = entry
                    .api
                    .or_else(|| config.api.clone())
                    .or_else(|| fallback.map(|model| model.api.clone()))
                    .unwrap_or_else(|| "openai-completions".into());
                let base_url = entry
                    .base_url
                    .or_else(|| config.base_url.clone())
                    .or_else(|| fallback.map(|model| model.base_url.clone()))
                    .unwrap_or_default();
                let mut headers = fallback.map(|model| model.headers.clone()).unwrap_or_default();
                headers.extend(config.headers.clone());
                headers.extend(entry.headers);
                let model = Model {
                    name: entry
                        .name
                        .or_else(|| base.as_ref().map(|model| model.name.clone()))
                        .unwrap_or_else(|| entry.id.clone()),
                    id: entry.id,
                    api,
                    provider: provider.clone(),
                    base_url,
                    reasoning: entry.reasoning.or(base.as_ref().map(|model| model.reasoning)).unwrap_or(false),
                    input: entry
                        .input
                        .or_else(|| base.as_ref().map(|model| model.input.clone()))
                        .unwrap_or_else(|| vec!["text".into()]),
                    cost: entry.cost.or(base.as_ref().map(|model| model.cost.clone())).unwrap_or_default(),
                    context_window: entry
                        .context_window
                        .or(base.as_ref().map(|model| model.context_window))
                        .unwrap_or(128_000),
                    max_tokens: entry.max_tokens.or(base.as_ref().map(|model| model.max_tokens)).unwrap_or(16_384),
                    headers,
                    compat: entry.compat.or_else(|| base.as_ref().and_then(|model| model.compat.clone())),
                    ..Default::default()
                };
                self.upsert(model);
            }
        }
        Ok(())
    }

    /// Add `model`, replacing one with the same provider and id.
    pub fn upsert(&mut self, model: Model) {
        match self.models.iter_mut().find(|existing| existing.provider == model.provider && existing.id == model.id) {
            Some(existing) => *existing = model,
            None => self.models.push(model),
        }
    }

    pub fn models(&self) -> &[Model] {
        &self.models
    }

    pub fn find(&self, provider: &str, id: &str) -> Option<&Model> {
        self.models.iter().find(|model| model.provider == provider && model.id == id)
    }

    /// Resolve `provider/id`, an exact id, or a case-insensitive substring of the id or name.
    pub fn resolve(&self, pattern: &str, provider: Option<&str>) -> Option<&Model> {
        let (provider, pattern) = match (provider, pattern.split_once('/')) {
            (None, Some((prefix, rest))) if self.models.iter().any(|model| model.provider == prefix) => {
                (Some(prefix), rest)
            }
            _ => (provider, pattern),
        };
        let candidates: Vec<&Model> =
            self.models.iter().filter(|model| provider.is_none_or(|provider| model.provider == provider)).collect();
        if let Some(model) = candidates.iter().find(|model| model.id == pattern) {
            return Some(model);
        }
        let needle = pattern.to_lowercase();
        let mut matches: Vec<&Model> = candidates
            .into_iter()
            .filter(|model| model.id.to_lowercase().contains(&needle) || model.name.to_lowercase().contains(&needle))
            .collect();
        matches.sort_by_key(|model| (model.id.len(), model.id.clone()));
        matches.into_iter().next()
    }

    /// API key for `provider`: `models.json` value (`$VAR`, `${VAR}`, `!command`, or literal), then environment.
    pub fn api_key(&self, provider: &str) -> Option<String> {
        self.api_keys.get(provider).and_then(|value| resolve_config_value(value)).or_else(|| get_env_api_key(provider))
    }

    /// Whether a key is configured for `provider`, without running `!command` values.
    pub fn has_api_key(&self, provider: &str) -> bool {
        self.api_keys.contains_key(provider) || get_env_api_key(provider).is_some()
    }
}

/// Resolve a configuration value: `$NAME` or `${NAME}` reads the environment,
/// `!command` runs a shell command and uses its trimmed output.
pub fn resolve_config_value(value: &str) -> Option<String> {
    if let Some(command) = value.strip_prefix('!') {
        let output = std::process::Command::new("sh").arg("-c").arg(command).output().ok()?;
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return (output.status.success() && !text.is_empty()).then_some(text);
    }
    let name =
        value.strip_prefix("${").and_then(|rest| rest.strip_suffix('}')).or_else(|| value.strip_prefix('$')).filter(
            |name| {
                !name.is_empty() && name.chars().all(|character| character.is_ascii_alphanumeric() || character == '_')
            },
        );
    match name {
        Some(name) => std::env::var(name).ok().filter(|value| !value.is_empty()),
        None => Some(value.to_string()),
    }
}

fn merge(target: &mut Value, patch: &Value) {
    match (target, patch) {
        (Value::Object(target), Value::Object(patch)) => {
            for (key, value) in patch {
                merge(target.entry(key.clone()).or_insert(Value::Null), value);
            }
        }
        (target, patch) => *target = patch.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_catalog_parses() {
        assert!(!builtin_models().is_empty());
        assert!(builtin_models().iter().all(|model| model.provider == "azure" || !model.base_url.is_empty()));
    }

    #[test]
    fn models_json_adds_providers_and_overrides() {
        let mut registry = ModelRegistry::builtin();
        let anthropic = registry.models().iter().find(|model| model.provider == "anthropic").unwrap().id.clone();
        registry
            .apply_json(&format!(
                r#"{{"providers":{{"ollama":{{"baseUrl":"http://localhost:11434/v1","api":"openai-completions","apiKey":"ollama","models":[{{"id":"qwen2.5-coder:7b"}}]}},
                "anthropic":{{"modelOverrides":{{"{anthropic}":{{"maxTokens":1234}}}}}}}}}}"#
            ))
            .unwrap();
        let ollama = registry.find("ollama", "qwen2.5-coder:7b").unwrap();
        assert_eq!(ollama.base_url, "http://localhost:11434/v1");
        assert_eq!(registry.api_key("ollama").as_deref(), Some("ollama"));
        assert_eq!(registry.find("anthropic", &anthropic).unwrap().max_tokens, 1234);
        assert_eq!(registry.resolve("ollama/qwen2.5-coder:7b", None).unwrap().provider, "ollama");
        assert_eq!(registry.resolve("QWEN", Some("ollama")).unwrap().id, "qwen2.5-coder:7b");
    }

    #[test]
    fn config_values_read_environment_and_commands() {
        assert_eq!(resolve_config_value("literal").as_deref(), Some("literal"));
        assert_eq!(resolve_config_value("!printf secret").as_deref(), Some("secret"));
        assert_eq!(resolve_config_value("$PI_RUST_SURELY_UNSET_VARIABLE"), None);
    }
}
