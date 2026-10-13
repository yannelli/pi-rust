//! The Radius gateway catalog: the account's `/v1/config` model list replaces the bundled
//! baseline once known, since organization owners can disable models (upstream
//! `ai/src/providers/radius.ts`).

use std::sync::RwLock;

use async_trait::async_trait;
use pi_ai::Model;
use pi_ai::auth::Credential;
use pi_ai::auth::oauth::{
    DEFAULT_RADIUS_GATEWAY, load_radius_gateway_config, normalize_radius_gateway_url, radius_models,
    radius_models_from_config,
};
use serde_json::Value;

use super::refresh::{DynamicCatalog, Persist, RefreshContext};
use super::store::ModelsStoreEntry;

pub const RADIUS_PROVIDER_ID: &str = "radius";

/// One Radius provider: the built-in `radius` or a `models.json` provider with `"oauth": "radius"`.
pub struct RadiusCatalog {
    provider_id: String,
    gateway: String,
    baseline: Vec<Model>,
    dynamic: RwLock<Option<Vec<Model>>>,
}

impl RadiusCatalog {
    /// `gateway` defaults to the public Radius gateway, whose bundled models form the baseline.
    pub fn new(provider_id: impl Into<String>, gateway: Option<&str>) -> Self {
        let provider_id = provider_id.into();
        let gateway = normalize_radius_gateway_url(gateway.unwrap_or(DEFAULT_RADIUS_GATEWAY));
        let baseline = if gateway == normalize_radius_gateway_url(DEFAULT_RADIUS_GATEWAY) {
            let bundled = pi_ai::models::builtin_models().iter().filter(|model| model.provider == RADIUS_PROVIDER_ID);
            bundled.map(|model| Model { provider: provider_id.clone(), ..model.clone() }).collect()
        } else {
            Vec::new()
        };
        Self { provider_id, gateway, baseline, dynamic: RwLock::new(None) }
    }

    pub fn gateway(&self) -> &str {
        &self.gateway
    }

    /// The gateway catalog when known, else the baseline.
    pub fn models(&self) -> Vec<Model> {
        let dynamic = self.dynamic.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        dynamic.clone().unwrap_or_else(|| self.baseline.clone())
    }

    fn set_dynamic(&self, models: Vec<Model>) {
        *self.dynamic.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(models);
    }
}

#[async_trait]
impl DynamicCatalog for RadiusCatalog {
    fn provider_id(&self) -> &str {
        &self.provider_id
    }

    fn compose(&self, _builtin: Vec<Model>) -> Vec<Model> {
        self.models()
    }

    async fn refresh(&self, context: &RefreshContext<'_>) -> anyhow::Result<()> {
        if let Some(stored) = context.stored {
            let restored: Vec<Model> =
                stored.models.iter().filter(|model| model.provider == self.provider_id).cloned().collect();
            if !context.publisher.publish(Persist::Unchanged).await {
                return Ok(());
            }
            self.set_dynamic(restored);
        }
        // Import catalogs that logins before the models store kept in the credential.
        if context.stored.is_none()
            && let Some(Credential::OAuth(credential)) = context.credential
        {
            let legacy = radius_models(&self.provider_id, Some(credential));
            if !legacy.is_empty() {
                let entry = ModelsStoreEntry {
                    checked_at: Some(pi_ai::now_ms()),
                    ..ModelsStoreEntry::with_models(legacy.clone())
                };
                if !context.publisher.publish(Persist::Write(entry)).await {
                    return Ok(());
                }
                self.set_dynamic(legacy);
            }
        }
        if !context.allow_network || context.cancel.is_cancelled() {
            return Ok(());
        }
        let api_key = match context.credential {
            Some(Credential::OAuth(credential)) => Some(credential.access.clone()),
            Some(Credential::ApiKey(credential)) => credential.key.clone(),
            None => None,
        };
        let config = load_radius_gateway_config(&self.gateway, api_key.as_deref(), context.cancel)
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        if context.cancel.is_cancelled() {
            return Ok(());
        }
        let refreshed = radius_models_from_config(&self.provider_id, &config);
        let entry =
            ModelsStoreEntry { checked_at: Some(pi_ai::now_ms()), ..ModelsStoreEntry::with_models(refreshed.clone()) };
        if context.publisher.publish(Persist::Write(entry)).await {
            self.set_dynamic(refreshed);
        }
        Ok(())
    }
}

/// A `models.json` provider served by a Radius gateway.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RadiusProviderConfig {
    pub id: String,
    pub name: String,
    /// The provider `baseUrl` without its `/v1` suffix.
    pub gateway: String,
}

fn strip_json_comments(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(ch) = chars.next() {
        if in_string {
            output.push(ch);
            match ch {
                '\\' => output.extend(chars.next()),
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match (ch, chars.peek()) {
            ('"', _) => {
                in_string = true;
                output.push(ch);
            }
            ('/', Some('/')) => while chars.next_if(|next| *next != '\n').is_some() {},
            ('/', Some('*')) => {
                chars.next();
                while let Some(next) = chars.next() {
                    if next == '*' && chars.next_if_eq(&'/').is_some() {
                        break;
                    }
                }
            }
            _ => output.push(ch),
        }
    }
    output
}

/// Providers in a `models.json` document with `"oauth": "radius"` and a `baseUrl`.
pub fn radius_providers_in_models_json(text: &str) -> Vec<RadiusProviderConfig> {
    let Ok(document) = serde_json::from_str::<Value>(&strip_json_comments(text)) else { return Vec::new() };
    let Some(providers) = document.get("providers").and_then(Value::as_object) else { return Vec::new() };
    let configs = providers.iter().filter_map(|(id, config)| {
        if config.get("oauth").and_then(Value::as_str) != Some("radius") {
            return None;
        }
        let base_url = config.get("baseUrl").and_then(Value::as_str).filter(|url| !url.is_empty())?;
        let trimmed = base_url.trim_end_matches('/');
        let gateway = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
        let name = config.get("name").and_then(Value::as_str).unwrap_or(id);
        Some(RadiusProviderConfig { id: id.clone(), name: name.into(), gateway: gateway.into() })
    });
    configs.collect()
}
