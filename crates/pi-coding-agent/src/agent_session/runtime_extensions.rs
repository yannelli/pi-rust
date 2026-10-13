//! Providers registered by extensions in the model runtime (upstream `ModelRuntime`
//! `registerProvider` and `unregisterProvider`): their models and auth settings apply over
//! `models.json`, and their OAuth flows join the built-in ones.

use std::collections::HashSet;
use std::sync::Arc;

use pi_ai::ModelRegistry;
use pi_ai::auth::OAuthAuth;
use serde_json::{Map, Value, json};

use super::model_runtime::{ModelRuntime, read, write};

const MODEL_KEYS: [&str; 7] = ["name", "baseUrl", "apiKey", "api", "headers", "authHeader", "models"];
const EFFECT_KEYS: [&str; 5] = ["baseUrl", "apiKey", "headers", "authHeader", "models"];

fn model_kind(model: &pi_ai::Model) -> String {
    model.model_type.clone().unwrap_or_else(|| "chat".into())
}

/// Apply one extension provider: its `models` replace the provider's models.
fn apply_extension_provider(registry: &mut ModelRegistry, provider: &str, config: &Value) -> Result<(), String> {
    let present = |key: &str| config.get(key).is_some_and(|value| !value.is_null());
    if !EFFECT_KEYS.iter().any(|key| present(key)) {
        return Ok(());
    }
    let fields: Map<String, Value> =
        MODEL_KEYS.iter().filter(|key| present(key)).map(|key| ((*key).to_string(), config[*key].clone())).collect();
    registry.apply_json(&json!({"providers": {provider: fields}}).to_string())?;
    if let Some(models) = config["models"].as_array() {
        let keep: HashSet<(String, String)> = models
            .iter()
            .filter_map(|model| {
                let kind = model["type"].as_str().unwrap_or("chat").to_string();
                Some((model["id"].as_str()?.to_string(), kind))
            })
            .collect();
        registry.retain_models(|model| model.provider != provider || keep.contains(&(model.id.clone(), model_kind(model))));
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
        apply_extension_provider(&mut probe, provider, &effective)?;
        let api_key = effective["apiKey"].as_str().and_then(pi_ai::models::resolve_config_value);
        write(&self.extension_providers).insert(provider.to_string(), effective);
        self.rebuild_registry();
        if api_key.is_some() {
            write(&self.configured).insert(provider.to_string());
            let configured = self.configured_providers();
            self.registry_mut().set_configured_providers(configured);
        }
        Ok(())
    }

    pub fn unregister_extension_provider(&self, provider: &str) {
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

    /// OAuth flows of extension providers, for `/login`.
    pub fn extension_oauth_providers(&self) -> Vec<(String, Arc<dyn OAuthAuth>)> {
        let mut providers: Vec<_> =
            read(&self.extension_oauth).iter().map(|(id, oauth)| (id.clone(), oauth.clone())).collect();
        providers.sort_by(|left, right| left.0.cmp(&right.0));
        providers
    }

    /// Apply the extension providers to a freshly composed registry.
    pub(super) fn apply_extension_providers(&self, registry: &mut ModelRegistry) {
        let providers = read(&self.extension_providers).clone();
        for (provider, config) in &providers {
            let _ = apply_extension_provider(registry, provider, config);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_models_replace_the_provider_models() {
        let mut registry = ModelRegistry::builtin();
        let config = json!({
            "baseUrl": "http://localhost:1",
            "api": "custom-api",
            "apiKey": "key",
            "models": [{"id": "only", "name": "Only", "contextWindow": 1000, "maxTokens": 100}],
        });
        apply_extension_provider(&mut registry, "anthropic", &config).unwrap();
        let models: Vec<&str> =
            registry.all_models().iter().filter(|model| model.provider == "anthropic").map(|model| model.id.as_str()).collect();
        assert_eq!(models, vec!["only"]);
        let model = registry.find("anthropic", "only").unwrap();
        assert_eq!((model.api.as_str(), model.base_url.as_str()), ("custom-api", "http://localhost:1"));
    }
}
