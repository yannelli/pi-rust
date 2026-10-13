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
    #[error("cannot read {path}: {source}")]
    Read { path: String, source: std::io::Error },
    #[error("invalid models file {path}: {message}")]
    Parse { path: String, message: String },
}

/// Available models and the `models.json` auth settings per provider.
#[derive(Clone, Debug, Default)]
pub struct ModelRegistry {
    models: Vec<Model>,
    auth_configs: HashMap<String, ProviderAuthConfig>,
    provider_names: HashMap<String, String>,
    /// Providers known to have usable auth; replaces the key heuristic when set.
    configured: Option<HashSet<String>>,
}

impl ModelRegistry {
    /// The bundled catalog only.
    pub fn builtin() -> Self {
        Self { models: builtin_models().to_vec(), ..Self::default() }
    }

    /// The bundled catalog plus `path` when it exists.
    pub fn load(path: &Path) -> Result<Self, ModelsFileError> {
        let mut registry = Self::builtin();
        if path.exists() {
            let display = path.display().to_string();
            let text = std::fs::read_to_string(path)
                .map_err(|source| ModelsFileError::Read { path: display.clone(), source })?;
            registry.apply_json(&text).map_err(|message| ModelsFileError::Parse { path: display, message })?;
        }
        Ok(registry)
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
