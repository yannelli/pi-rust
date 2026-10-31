//! Providers registered by extensions in the model runtime (upstream `ModelRuntime`
//! `registerProvider` and `provider-composer.ts`): their models and auth settings apply over
//! `models.json`, `modelOverrides` apply last, and their OAuth flows join the built-in ones.

use std::sync::Arc;

use pi_ai::auth::OAuthAuth;
use pi_ai::{Model, ModelRegistry};
use serde_json::{Map, Value, json};

use super::model_runtime::{ModelRuntime, read, write};
pub use super::runtime_extension_ops::{ExtensionProviderHooks, RefreshModelsContext};

/// Keys of a registration that change the composed provider.
const EFFECT_KEYS: [&str; 6] = ["name", "baseUrl", "apiKey", "headers", "authHeader", "models"];

pub(super) fn model_kind(model: &Model) -> &str {
    model.model_type.as_deref().unwrap_or("chat")
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// Upstream `findExtensionModelDefaults`: a model of the same type to inherit `api` and
/// `baseUrl` from.
fn extension_defaults<'a>(models: &'a [Model], definition: &Value) -> Option<&'a Model> {
    let kind = text(definition, "type").unwrap_or("chat");
    let candidates: Vec<&Model> = models.iter().filter(|model| model_kind(model) == kind).collect();
    let id = text(definition, "id").unwrap_or_default();
    let api = text(definition, "api");
    candidates
        .iter()
        .find(|model| model.id == id)
        .or_else(|| api.and_then(|api| candidates.iter().find(|model| model.api == api)))
        .or_else(
            || if kind == "chat" { candidates.iter().find(|model| model.api == "openai-completions") } else { None },
        )
        .or_else(|| candidates.first())
        .copied()
}

/// Upstream `extensionModelFromDefinition`: the definition as given, with `api`, `provider`, and
/// `baseUrl` filled in and `headers` moved to the request auth.
fn extension_model(provider: &str, models: &[Model], config: &Value, definition: &Value) -> Result<Model, String> {
    let kind = text(definition, "type").unwrap_or("chat");
    let id = text(definition, "id").unwrap_or_default().to_string();
    let defaults = extension_defaults(models, definition);
    let provider_api = if kind == "chat" { text(config, "api") } else { None };
    let api = text(definition, "api").or(provider_api).map(str::to_string).or_else(|| defaults.map(|m| m.api.clone()));
    let api = api.ok_or_else(|| {
        let level = if kind == "chat" { " or provider level" } else { "" };
        format!("Provider {provider}, model {id}: no \"api\" specified. Set it at model level{level}.")
    })?;
    let base_url = text(definition, "baseUrl")
        .or_else(|| text(config, "baseUrl"))
        .map(str::to_string)
        .or_else(|| defaults.map(|model| model.base_url.clone()))
        .ok_or_else(|| format!("Provider {provider}: \"baseUrl\" is required when defining custom models."))?;
    let mut object: Map<String, Value> = definition.as_object().cloned().unwrap_or_default();
    object.insert("api".into(), json!(api));
    object.insert("provider".into(), json!(provider));
    object.insert("baseUrl".into(), json!(base_url));
    object.remove("headers");
    // The Rust model needs a name and marks chat models by an absent type.
    object.entry("name").or_insert_with(|| json!(id));
    if kind == "chat" {
        object.remove("type");
    }
    serde_json::from_value(Value::Object(object)).map_err(|error| format!("Provider {provider}, model {id}: {error}"))
}

/// Upstream `applyExtension` plus the auth layering of `composeModelProvider`: `models` (or the
/// refreshed list) replace the provider's models; otherwise `baseUrl` moves the existing ones.
pub(super) fn apply_extension_provider(
    registry: &mut ModelRegistry,
    provider: &str,
    config: &Value,
    refreshed: Option<&[Value]>,
) -> Result<(), String> {
    let present = |key: &str| config.get(key).is_some_and(|value| !value.is_null());
    if refreshed.is_none() && !EFFECT_KEYS.iter().any(|key| present(key)) {
        return Ok(());
    }
    let current = registry.provider_models(provider);
    let definitions = refreshed.or_else(|| config["models"].as_array().map(Vec::as_slice));
    let models = match definitions {
        Some(definitions) => definitions
            .iter()
            .map(|definition| extension_model(provider, &current, config, definition))
            .collect::<Result<Vec<_>, _>>()?,
        None => match text(config, "baseUrl") {
            Some(base_url) => current.into_iter().map(|model| Model { base_url: base_url.into(), ..model }).collect(),
            None => current,
        },
    };
    registry.replace_provider_models(provider, models);
    if let Some(name) = text(config, "name") {
        registry.set_provider_name(provider, name);
    }
    let auth = registry.auth_config_mut(provider);
    if let Some(key) = text(config, "apiKey") {
        auth.api_key = Some(key.to_string());
    }
    if let Some(headers) = config["headers"].as_object() {
        auth.headers.extend(headers.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))));
    }
    if let Some(auth_header) = config["authHeader"].as_bool() {
        auth.auth_header = auth_header;
    }
    for definition in definitions.into_iter().flatten() {
        let (Some(id), Some(headers)) = (text(definition, "id"), definition["headers"].as_object()) else { continue };
        let entry = auth.model_headers.entry(id.to_string()).or_default();
        entry.extend(headers.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))));
    }
    Ok(())
}

impl ModelRuntime {
    /// Register or update an extension provider; defined values merge over the last
    /// registration. An invalid config changes nothing.
    pub fn register_extension_provider(&self, provider: &str, config: Value) -> Result<(), String> {
        let mut effective = read(&self.extension_providers).get(provider).cloned().unwrap_or_else(|| json!({}));
        for (key, value) in config.as_object().into_iter().flatten() {
            if !value.is_null() {
                effective[key] = value.clone();
            }
        }
        let mut probe = self.registry().clone();
        apply_extension_provider(&mut probe, provider, &effective, None)?;
        let api_key = effective["apiKey"].as_str().and_then(pi_ai::models::resolve_config_value);
        write(&self.extension_providers).insert(provider.to_string(), effective);
        if let Some(state) = write(&self.extension_hooks).get_mut(provider) {
            state.refreshed = None;
        }
        self.rebuild_registry();
        if api_key.is_some() {
            write(&self.configured).insert(provider.to_string());
            let configured = self.configured_providers();
            self.registry_mut().set_configured_providers(configured);
        }
        Ok(())
    }

    pub fn unregister_extension_provider(&self, provider: &str) {
        write(&self.extension_hooks).remove(provider);
        if write(&self.extension_providers).remove(provider).is_some() {
            self.rebuild_registry();
        }
    }

    /// Set or remove the OAuth flow of an extension provider.
    pub fn register_extension_oauth(&self, provider: &str, oauth: Option<Arc<dyn OAuthAuth>>) {
        let mut registered = write(&self.extension_oauth);
        match oauth {
            Some(oauth) => drop(registered.insert(provider.to_string(), oauth)),
            None => drop(registered.remove(provider)),
        }
    }

    /// The core OAuth flow of provider `id` (`oauth: { builtin }` of `pi.registerProvider`).
    pub fn builtin_oauth(&self, id: &str) -> Option<Arc<dyn OAuthAuth>> {
        (self.builtin_oauth)(id)
    }

    /// OAuth flows of extension providers, for `/login`.
    pub fn extension_oauth_providers(&self) -> Vec<(String, Arc<dyn OAuthAuth>)> {
        let mut providers: Vec<_> =
            read(&self.extension_oauth).iter().map(|(id, oauth)| (id.clone(), oauth.clone())).collect();
        providers.sort_by(|left, right| left.0.cmp(&right.0));
        providers
    }

    /// Apply the extension providers to a freshly composed registry, then the `modelOverrides`
    /// of `models.json` as the topmost layer.
    pub(super) fn apply_extension_providers(&self, registry: &mut ModelRegistry) {
        let providers = read(&self.extension_providers).clone();
        let hooks = read(&self.extension_hooks);
        for (provider, config) in &providers {
            let state = hooks.get(provider);
            let refreshed = state.and_then(|state| state.refreshed.as_deref());
            if apply_extension_provider(registry, provider, config, refreshed).is_err() && refreshed.is_some() {
                let _ = apply_extension_provider(registry, provider, config, None);
            }
            if let Some(modified) = state.and_then(|state| state.modified.clone()) {
                let others: Vec<Model> = registry
                    .provider_models(provider)
                    .into_iter()
                    .filter(|model| model_kind(model) != "chat")
                    .collect();
                registry.replace_provider_models(provider, modified.into_iter().chain(others).collect());
            }
            let _ = registry.apply_model_overrides(provider);
        }
    }
}

#[cfg(test)]
#[path = "runtime_extensions_tests.rs"]
mod tests;
