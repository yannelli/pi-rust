//! Bundled model catalog, the `models.json` overlay used by the TypeScript pi, and
//! per-model thinking level support.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

use crate::auth::ProviderAuthConfig;
use crate::env::get_env_api_key;
use crate::types::Model;

#[path = "models_json.rs"]
mod json;

static BUILTIN: LazyLock<Vec<Model>> =
    LazyLock::new(|| serde_json::from_str(include_str!("../models.json")).expect("bundled models.json is valid"));

/// Models shipped with this crate.
pub fn builtin_models() -> &'static [Model] {
    &BUILTIN
}

#[derive(Debug, thiserror::Error)]
pub enum ModelsFileError {
    #[error("Failed to load models.json: {source}\n\nFile: {path}")]
    Read { path: String, source: std::io::Error },
    /// The text is not JSON (comments allowed).
    #[error("Failed to parse models.json: {message}\n\nFile: {path}")]
    Syntax { path: String, message: String },
    /// The document does not match the `models.json` schema; one `path: message` per line.
    #[error("Invalid models.json schema:\n{}\n\nFile: {path}", format_schema_errors(.errors))]
    Schema { path: String, errors: Vec<String> },
    /// A provider configuration that cannot be applied, such as a model without `api`.
    #[error("invalid models file {path}: {message}")]
    Parse { path: String, message: String },
}

fn format_schema_errors(errors: &[String]) -> String {
    if errors.is_empty() {
        return "Unknown schema error".into();
    }
    errors.iter().map(|error| format!("  - {error}")).collect::<Vec<_>>().join("\n")
}

static MODELS_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("models.schema.json")).expect("bundled models schema is valid"));

/// Check `models.json` text the way upstream `ModelConfig.load` does: comments and a BOM are
/// allowed, then the document must match the schema. `path` names the file in the error.
pub fn validate_models_json(text: &str, path: &str) -> Result<(), ModelsFileError> {
    let stripped = json::strip_json_comments(text.trim_start_matches('\u{feff}'));
    let value: serde_json::Value = serde_json::from_str(&stripped)
        .map_err(|error| ModelsFileError::Syntax { path: path.into(), message: error.to_string() })?;
    let errors = crate::json_schema::schema_errors(&MODELS_SCHEMA, &value);
    if errors.is_empty() { Ok(()) } else { Err(ModelsFileError::Schema { path: path.into(), errors }) }
}

/// The `baseUrl` of each provider in a `models.json` document; empty when it is invalid.
pub fn provider_base_urls(text: &str) -> HashMap<String, String> {
    let Ok(file) = json::parse_models_file(text) else { return HashMap::new() };
    let urls = file.providers.into_iter().filter_map(|(id, config)| Some((id, config.base_url?)));
    urls.filter(|(_, url)| !url.trim().is_empty()).collect()
}

/// Available models and the `models.json` auth settings per provider.
#[derive(Clone, Debug, Default)]
pub struct ModelRegistry {
    models: Vec<Model>,
    auth_configs: HashMap<String, ProviderAuthConfig>,
    provider_names: HashMap<String, String>,
    /// Providers known to have usable auth; replaces the key heuristic when set.
    configured: Option<HashSet<String>>,
    /// `models.json` `modelOverrides` per provider, applied again after extension providers.
    model_overrides: HashMap<String, std::collections::BTreeMap<String, serde_json::Map<String, serde_json::Value>>>,
}

impl ModelRegistry {
    /// The bundled catalog only.
    pub fn builtin() -> Self {
        Self { models: builtin_models().to_vec(), ..Self::default() }
    }

    /// The bundled catalog plus `path` when it exists.
    pub fn load(path: &Path) -> Result<Self, ModelsFileError> {
        let mut registry = Self::builtin();
        registry.apply_models_file(path)?;
        Ok(registry)
    }

    /// Validate and apply the `models.json` at `path`; a missing file changes nothing.
    pub fn apply_models_file(&mut self, path: &Path) -> Result<(), ModelsFileError> {
        if !path.exists() {
            return Ok(());
        }
        let display = path.display().to_string();
        let text =
            std::fs::read_to_string(path).map_err(|source| ModelsFileError::Read { path: display.clone(), source })?;
        validate_models_json(&text, &display)?;
        self.apply_json(&text).map_err(|message| ModelsFileError::Parse { path: display, message })
    }

    /// Like [`Self::load`], but an invalid file leaves the bundled catalog and returns the error,
    /// as upstream reports `models.json` problems without stopping.
    pub fn load_lenient(path: &Path) -> (Self, Option<ModelsFileError>) {
        match Self::load(path) {
            Ok(registry) => (registry, None),
            Err(error) => (Self::builtin(), Some(error)),
        }
    }

    /// Apply a `models.json` document (comments allowed). Nothing changes when it is invalid.
    pub fn apply_json(&mut self, text: &str) -> Result<(), String> {
        let file = json::parse_models_file(text)?;
        let mut models = self.models.clone();
        let mut auth_configs = self.auth_configs.clone();
        for (provider, config) in &file.providers {
            let (own, rest): (Vec<Model>, Vec<Model>) =
                models.into_iter().partition(|model| &model.provider == provider);
            let applied = json::apply_provider(provider, own, config)?;
            models = rest;
            models.extend(applied);
            auth_configs.insert(provider.clone(), json::auth_config(config));
            if let Some(overrides) = config.model_overrides.as_ref().filter(|overrides| !overrides.is_empty()) {
                self.model_overrides.insert(provider.clone(), overrides.clone());
            }
            if let Some(name) = &config.name {
                self.provider_names.insert(provider.clone(), name.clone());
            }
        }
        self.models = models;
        self.auth_configs = auth_configs;
        Ok(())
    }

    /// Add `model`, replacing one with the same provider, id, and type (a chat model and a
    /// classifier may share an id).
    pub fn upsert(&mut self, model: Model) {
        let kind = |model: &Model| model.model_type.clone().unwrap_or_else(|| "chat".into());
        let same = |existing: &Model| {
            existing.provider == model.provider && existing.id == model.id && kind(existing) == kind(&model)
        };
        match self.models.iter_mut().find(|existing| same(existing)) {
            Some(existing) => *existing = model,
            None => self.models.push(model),
        }
    }

    /// Every model of `provider`, of every type, in catalog order.
    pub fn provider_models(&self, provider: &str) -> Vec<Model> {
        self.models.iter().filter(|model| model.provider == provider).cloned().collect()
    }

    /// Replace the models of `provider` with `models`.
    pub fn replace_provider_models(&mut self, provider: &str, models: Vec<Model>) {
        self.models.retain(|model| model.provider != provider);
        self.models.extend(models);
    }

    /// Apply the `models.json` `modelOverrides` of `provider` to its chat models (upstream
    /// applies them last, after custom models and extension providers). Merging is idempotent.
    pub fn apply_model_overrides(&mut self, provider: &str) -> Result<(), String> {
        let Some(overrides) = self.model_overrides.get(provider) else { return Ok(()) };
        for (id, patch) in overrides {
            for model in self.models.iter_mut().filter(|model| model.provider == provider && &model.id == id) {
                if is_chat_model(model) {
                    *model = json::apply_override(model, patch)
                        .map_err(|error| format!("Provider {provider}, model {id}: {error}"))?;
                }
            }
        }
        Ok(())
    }

    /// The auth settings of `provider`, created empty when missing.
    pub fn auth_config_mut(&mut self, provider: &str) -> &mut ProviderAuthConfig {
        self.auth_configs.entry(provider.to_string()).or_default()
    }

    /// Set the display name of `provider`.
    pub fn set_provider_name(&mut self, provider: &str, name: &str) {
        self.provider_names.insert(provider.to_string(), name.to_string());
    }

    /// Keep only the models `keep` accepts.
    pub fn retain_models(&mut self, keep: impl Fn(&Model) -> bool) {
        self.models.retain(|model| keep(model));
    }

    /// Every model, including image and classifier models.
    pub fn all_models(&self) -> &[Model] {
        &self.models
    }

    /// Chat models.
    pub fn models(&self) -> Vec<&Model> {
        self.models.iter().filter(|model| is_chat_model(model)).collect()
    }

    /// Chat models whose provider has configured auth.
    pub fn available_models(&self) -> Vec<&Model> {
        self.models.iter().filter(|model| is_chat_model(model) && self.has_api_key(&model.provider)).collect()
    }

    /// Distinct provider ids in catalog order.
    pub fn providers(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut providers: Vec<String> = self
            .models
            .iter()
            .filter(|model| seen.insert(model.provider.clone()))
            .map(|model| model.provider.clone())
            .collect();
        for provider in self.auth_configs.keys() {
            if seen.insert(provider.clone()) {
                providers.push(provider.clone());
            }
        }
        providers
    }

    pub fn has_provider(&self, provider: &str) -> bool {
        self.models.iter().any(|model| model.provider == provider) || self.auth_configs.contains_key(provider)
    }

    /// Display name from `models.json`, if set.
    pub fn provider_name(&self, provider: &str) -> Option<&str> {
        self.provider_names.get(provider).map(String::as_str)
    }

    /// The chat model `provider`/`id`.
    pub fn find(&self, provider: &str, id: &str) -> Option<&Model> {
        self.models.iter().find(|model| model.provider == provider && model.id == id && is_chat_model(model))
    }

    /// `models.json` auth settings per provider, for [`crate::auth::AuthResolver`].
    pub fn auth_configs(&self) -> HashMap<String, ProviderAuthConfig> {
        self.auth_configs.clone()
    }

    /// Resolve `provider/id`, an exact id, or a case-insensitive substring of the id or name.
    pub fn resolve(&self, pattern: &str, provider: Option<&str>) -> Option<&Model> {
        let (provider, pattern) = match (provider, pattern.split_once('/')) {
            (None, Some((prefix, rest))) if self.models.iter().any(|model| model.provider == prefix) => {
                (Some(prefix), rest)
            }
            _ => (provider, pattern),
        };
        let candidates: Vec<&Model> = self
            .models
            .iter()
            .filter(|model| is_chat_model(model) && provider.is_none_or(|provider| model.provider == provider))
            .collect();
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

    /// Record which providers have usable auth (from [`crate::auth::AuthResolver::check`]).
    pub fn set_configured_providers(&mut self, providers: HashSet<String>) {
        self.configured = Some(providers);
    }

    /// Synchronous API key lookup: `models.json` value (`$VAR`, `!command`, or literal), then
    /// environment. Stored `auth.json` credentials and OAuth need [`crate::auth::AuthResolver`].
    pub fn api_key(&self, provider: &str) -> Option<String> {
        let configured = self.auth_configs.get(provider).and_then(|config| config.api_key.as_deref());
        configured.and_then(resolve_config_value).or_else(|| get_env_api_key(provider))
    }

    /// Whether `provider` has usable auth. Uses the recorded configured providers when set,
    /// otherwise a `models.json` key or an environment variable, without running commands.
    pub fn has_api_key(&self, provider: &str) -> bool {
        match &self.configured {
            Some(configured) => configured.contains(provider),
            None => {
                self.auth_configs.get(provider).is_some_and(|config| config.api_key.is_some())
                    || get_env_api_key(provider).is_some()
                    || crate::env::find_env_keys(provider, None).is_some()
            }
        }
    }
}

/// Resolve a configuration value against the process environment. See
/// [`crate::auth::resolve_value::resolve_config_value`].
pub fn resolve_config_value(value: &str) -> Option<String> {
    crate::auth::resolve_value::resolve_config_value(value, None)
}

/// Whether `model` is a chat model (no `type`, or `chat`), as opposed to image or classifier models.
pub fn is_chat_model(model: &Model) -> bool {
    model.model_type.as_deref().is_none_or(|kind| kind == "chat")
}

pub use crate::simple_options::{clamp_thinking_level, supported_thinking_levels};

/// Whether two models are the same catalog entry.
pub fn models_are_equal(a: &Model, b: &Model) -> bool {
    a.provider == b.provider && a.id == b.id && a.model_type == b.model_type
}

#[cfg(test)]
#[path = "models_tests.rs"]
mod tests;
