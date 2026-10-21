//! The pi.dev catalog overlay for built-in providers: newer models are fetched from
//! `/api/models/providers/<id>`, cached in `models-store.json` with their `ETag`, and merged
//! over the bundled list (upstream `core/remote-catalog-provider.ts`).

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;

use async_trait::async_trait;
use pi_ai::Model;
use serde_json::Value;

use super::refresh::{DynamicCatalog, Persist, RefreshContext};
use super::store::{KNOWN_MODEL_TYPES, ModelsStoreEntry, model_type};
use crate::platform::http::{FetchRetryOptions, VERSION, fetch_with_retry, pi_user_agent};

pub const DEFAULT_CATALOG_BASE_URL: &str = "https://pi.dev";
/// Stored catalogs younger than this are not fetched again unless forced.
pub const REMOTE_CATALOG_REFRESH_INTERVAL_MS: i64 = 4 * 60 * 60 * 1000;
const ATTEMPT_TIMEOUT: Duration = Duration::from_millis(4_000);

/// Merge by model type and id: `dynamic` replaces matching baseline entries in place and
/// appends new ones.
pub fn merge_models(baseline: Vec<Model>, dynamic: &[Model]) -> Vec<Model> {
    let mut merged: Vec<Model> = Vec::with_capacity(baseline.len() + dynamic.len());
    let mut index: HashMap<(String, String), usize> = HashMap::new();
    for model in baseline.into_iter().chain(dynamic.iter().cloned()) {
        let key = (model_type(&model).to_string(), model.id.clone());
        match index.get(&key) {
            Some(&position) => merged[position] = model,
            None => {
                index.insert(key, merged.len());
                merged.push(model);
            }
        }
    }
    merged
}

/// Models of a catalog response: an array, `{ models: [...] }`, or an object keyed by id.
/// Entries of unknown types are dropped and `provider` is set to `provider_id`.
pub fn parse_catalog(provider_id: &str, value: &Value) -> Result<Vec<Model>, String> {
    let entries: Vec<&Value> = match value {
        Value::Array(items) => items.iter().collect(),
        Value::Object(map) => match map.get("models") {
            Some(Value::Array(items)) => items.iter().collect(),
            _ => map.values().collect(),
        },
        _ => return Err(format!("Invalid model catalog for provider \"{provider_id}\"")),
    };
    let models = entries.into_iter().filter_map(|entry| {
        let object = entry.as_object().filter(|object| object.contains_key("id"))?;
        let supported = match object.get("type") {
            None => true,
            Some(Value::String(kind)) => KNOWN_MODEL_TYPES.contains(&kind.as_str()),
            Some(_) => false,
        };
        if !supported {
            return None;
        }
        let mut object = object.clone();
        object.insert("provider".into(), Value::String(provider_id.into()));
        serde_json::from_value(Value::Object(object)).ok()
    });
    Ok(models.collect())
}

/// Whether a pi.dev catalog is newer than the model data bundled with pi.
pub fn is_newer(last_modified: Option<i64>, local_generated_at: Option<i64>) -> bool {
    match local_generated_at {
        None => true,
        Some(local) => last_modified.is_some_and(|remote| remote > local),
    }
}

/// The stored overlay, unless the bundled catalog is at least as new.
pub fn remote_models(entry: Option<&ModelsStoreEntry>, local_generated_at: Option<i64>) -> Vec<Model> {
    match entry {
        Some(entry) if is_newer(entry.last_modified, local_generated_at) => entry.models.clone(),
        _ => Vec::new(),
    }
}

fn encode_uri_component(text: &str) -> String {
    let mut encoded = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Unix milliseconds of an HTTP date such as `Wed, 23 Jul 2026 10:01:00 GMT`.
pub fn parse_http_date(value: &str) -> Option<i64> {
    let parts: Vec<&str> = value.split_whitespace().collect();
    let [_, day, month, year, time, zone] = parts.as_slice() else { return None };
    if !matches!(*zone, "GMT" | "UTC") {
        return None;
    }
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let month = MONTHS.iter().position(|name| name.eq_ignore_ascii_case(month))? as i64 + 1;
    let (day, year): (i64, i64) = (day.parse().ok()?, year.parse().ok()?);
    let mut clock = time.split(':').map(|part| part.parse::<i64>().ok());
    let (hours, minutes, seconds) = (clock.next()??, clock.next()??, clock.next()??);
    if !(1..=31).contains(&day) || hours > 23 || minutes > 59 || seconds > 60 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(((days * 24 + hours) * 60 + minutes) * 60_000 + seconds * 1000)
}

/// `GET <base>/api/models/providers/<id>?types=...`, revalidating with `validator` when given.
pub(super) async fn request_catalog(
    client: &reqwest::Client,
    base_url: &str,
    provider_id: &str,
    validator: Option<&str>,
    cancel: &tokio_util::sync::CancellationToken,
) -> anyhow::Result<reqwest::Response> {
    let types = KNOWN_MODEL_TYPES.join(",");
    let url = format!("{base_url}/api/models/providers/{}?types={types}", encode_uri_component(provider_id));
    let agent = pi_user_agent(VERSION);
    let build = || {
        let request = client.get(&url).header("accept", "application/json").header("User-Agent", &agent);
        match validator {
            Some(etag) => request.header("if-none-match", etag),
            None => request,
        }
    };
    let options = FetchRetryOptions { attempt_timeout: Some(ATTEMPT_TIMEOUT), ..FetchRetryOptions::default() };
    Ok(fetch_with_retry(build, &options, cancel).await?)
}

/// A successful catalog response as a store entry without `checked_at`; a missing or invalid
/// `Last-Modified` header is 0.
pub(super) async fn read_catalog(provider_id: &str, response: reqwest::Response) -> anyhow::Result<ModelsStoreEntry> {
    let header = |name: &str| response.headers().get(name).and_then(|value| value.to_str().ok()).map(String::from);
    let (last_modified, etag) = (header("last-modified"), header("etag"));
    let body: Value = response.json().await?;
    let models = parse_catalog(provider_id, &body).map_err(anyhow::Error::msg)?;
    let last_modified = Some(last_modified.as_deref().and_then(parse_http_date).unwrap_or(0));
    Ok(ModelsStoreEntry { models, checked_at: None, last_modified, etag })
}

/// The pi.dev overlay of one built-in provider.
pub struct RemoteCatalogOverlay {
    provider_id: String,
    base_url: String,
    local_generated_at: Option<i64>,
    client: reqwest::Client,
    dynamic: RwLock<Vec<Model>>,
}

impl RemoteCatalogOverlay {
    pub fn new(provider_id: impl Into<String>, base_url: Option<&str>, local_generated_at: Option<i64>) -> Self {
        Self {
            provider_id: provider_id.into(),
            base_url: base_url.unwrap_or(DEFAULT_CATALOG_BASE_URL).trim_end_matches('/').to_string(),
            local_generated_at,
            client: pi_ai::http::shared_client(),
            dynamic: RwLock::new(Vec::new()),
        }
    }

    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    /// The overlay models currently in effect.
    pub fn overlay_models(&self) -> Vec<Model> {
        self.dynamic.read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    fn set_dynamic(&self, models: Vec<Model>) {
        *self.dynamic.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = models;
    }
}

#[async_trait]
impl DynamicCatalog for RemoteCatalogOverlay {
    fn provider_id(&self) -> &str {
        &self.provider_id
    }

    fn compose(&self, builtin: Vec<Model>) -> Vec<Model> {
        merge_models(builtin, &self.overlay_models())
    }

    async fn refresh(&self, context: &RefreshContext<'_>) -> anyhow::Result<()> {
        let stored = context.stored;
        let mut restored = remote_models(stored, self.local_generated_at);
        restored.retain(|model| model.provider == self.provider_id);
        if !context.publisher.publish(Persist::Unchanged).await {
            return Ok(());
        }
        self.set_dynamic(restored);
        if !context.allow_network || context.cancel.is_cancelled() {
            return Ok(());
        }
        if !context.force
            && let Some(entry) = stored
            && let (Some(checked_at), Some(_)) = (entry.checked_at, entry.last_modified)
            && pi_ai::now_ms() - checked_at < REMOTE_CATALOG_REFRESH_INTERVAL_MS
        {
            return Ok(());
        }
        // Revalidate only when a cached body backs the validator, so a 304 cannot empty the overlay.
        let validator = stored.filter(|entry| !entry.models.is_empty()).and_then(|entry| entry.etag.as_deref());
        let response =
            request_catalog(&self.client, &self.base_url, &self.provider_id, validator, context.cancel).await?;
        if context.cancel.is_cancelled() {
            return Ok(());
        }
        let checked_at = Some(pi_ai::now_ms());
        let status = response.status().as_u16();
        let base = || stored.cloned().unwrap_or_default();
        if status == 304
            && let Some(stored) = stored
        {
            context.publisher.publish(Persist::Write(ModelsStoreEntry { checked_at, ..stored.clone() })).await;
            return Ok(());
        }
        if status == 404 || status == 501 {
            let entry = ModelsStoreEntry { checked_at, last_modified: Some(0), etag: None, ..base() };
            context.publisher.publish(Persist::Write(entry)).await;
            return Ok(());
        }
        if !response.status().is_success() {
            // The cached body and its validator stay valid, so the next refresh can revalidate.
            context.publisher.publish(Persist::Write(ModelsStoreEntry { checked_at, ..base() })).await;
            anyhow::bail!("Model catalog request failed for {}: {status}", self.provider_id);
        }
        let entry = ModelsStoreEntry { checked_at, ..read_catalog(&self.provider_id, response).await? };
        if context.cancel.is_cancelled() {
            return Ok(());
        }
        let published = remote_models(Some(&entry), self.local_generated_at);
        if context.publisher.publish(Persist::Write(entry)).await {
            self.set_dynamic(published);
        }
        Ok(())
    }
}
