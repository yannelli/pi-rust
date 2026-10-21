//! Account-scoped catalogs: each network refresh narrows the catalog (bundled models plus a newer
//! pi.dev catalog) to the models the credential may use and saves the result, which then replaces
//! the catalog, offline too (upstream `withPersonalizedCatalog`).

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use futures::future::BoxFuture;
use pi_ai::Model;
use pi_ai::auth::Credential;
use pi_ai::model_lists::{AccountModelList, ModelListContext};

use super::refresh::{DynamicCatalog, Persist, RefreshContext};
use super::remote::{
    DEFAULT_CATALOG_BASE_URL, REMOTE_CATALOG_REFRESH_INTERVAL_MS, is_newer, merge_models, read_catalog, remote_models,
    request_catalog,
};
use super::store::ModelsStoreEntry;
use crate::platform::http::{VERSION, pi_user_agent};

/// Narrows a catalog to a credential's models. An error keeps the last saved list.
pub type Personalize =
    Arc<dyn Fn(Vec<Model>, ModelListContext) -> BoxFuture<'static, Result<Vec<Model>, String>> + Send + Sync>;

fn is_overlay_entry(entry: &ModelsStoreEntry) -> bool {
    entry.last_modified.is_some() || entry.etag.is_some()
}

/// The account model list of a built-in provider, and the minimum time between its requests:
/// none for upstream's lists, the catalog interval for pi-rust's.
pub fn account_model_list(provider_id: &str) -> Option<(Personalize, Option<i64>)> {
    let list = AccountModelList::for_provider(provider_id)?;
    let personalize: Personalize =
        Arc::new(move |catalog, context| Box::pin(async move { list.personalize(catalog, &context).await }));
    Some((personalize, (!list.is_upstream()).then_some(REMOTE_CATALOG_REFRESH_INTERVAL_MS)))
}

pub struct PersonalizedCatalog {
    provider_id: String,
    builtin: Vec<Model>,
    personalize: Personalize,
    catalog_base_url: String,
    local_generated_at: Option<i64>,
    provider_base_url: Option<String>,
    interval: Option<i64>,
    client: reqwest::Client,
    personal: RwLock<Vec<Model>>,
}

impl PersonalizedCatalog {
    /// `builtin` holds the provider's bundled models.
    pub fn new(provider_id: impl Into<String>, builtin: Vec<Model>, personalize: Personalize) -> Self {
        Self {
            provider_id: provider_id.into(),
            builtin,
            personalize,
            catalog_base_url: DEFAULT_CATALOG_BASE_URL.into(),
            local_generated_at: None,
            provider_base_url: None,
            interval: None,
            client: pi_ai::http::shared_client(),
            personal: RwLock::new(Vec::new()),
        }
    }

    /// The pi.dev catalog server, and the generation time of the bundled models.
    pub fn with_catalog(mut self, base_url: Option<&str>, local_generated_at: Option<i64>) -> Self {
        self.catalog_base_url = base_url.unwrap_or(DEFAULT_CATALOG_BASE_URL).trim_end_matches('/').to_string();
        self.local_generated_at = local_generated_at;
        self
    }

    /// The provider `baseUrl` from `models.json`, where the model list is requested.
    pub fn with_provider_base_url(mut self, base_url: Option<String>) -> Self {
        self.provider_base_url = base_url;
        self
    }

    /// Skip network refreshes within `interval_ms` of the last saved list, unless forced.
    pub fn with_interval(mut self, interval_ms: Option<i64>) -> Self {
        self.interval = interval_ms;
        self
    }

    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    /// The saved list, or the bundled models while it is empty.
    pub fn models(&self) -> Vec<Model> {
        let personal = self.personal.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        if personal.is_empty() { self.builtin.clone() } else { personal.clone() }
    }

    fn set_personal(&self, models: Vec<Model>) {
        *self.personal.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = models;
    }

    /// Whether the saved list is recent enough to skip this refresh. Entries with pi.dev
    /// validators were saved by the plain overlay and are always refreshed.
    fn is_fresh(&self, context: &RefreshContext<'_>) -> bool {
        let (Some(interval), Some(entry)) = (self.interval, context.stored) else { return false };
        let saved_here = !is_overlay_entry(entry) && !entry.models.is_empty();
        !context.force && saved_here && entry.checked_at.is_some_and(|at| pi_ai::now_ms() - at < interval)
    }

    /// The saved list. pi-rust's lists read an entry of the plain overlay, saved before the
    /// provider had a list, as an overlay; upstream restores every entry as it is.
    fn restore(&self, stored: Option<&ModelsStoreEntry>) -> Vec<Model> {
        let models = match stored {
            Some(entry) if self.interval.is_some() && is_overlay_entry(entry) => {
                merge_models(self.builtin.clone(), &remote_models(Some(entry), self.local_generated_at))
            }
            Some(entry) => entry.models.clone(),
            None => Vec::new(),
        };
        models.into_iter().filter(|model| model.provider == self.provider_id).collect()
    }

    fn list_context(&self, context: &RefreshContext<'_>) -> ModelListContext {
        let key = match context.credential {
            Some(Credential::ApiKey(credential)) => credential.key.clone(),
            Some(Credential::OAuth(credential)) => Some(credential.access.clone()),
            None => None,
        };
        ModelListContext {
            key,
            base_url: self.provider_base_url.clone(),
            client: self.client.clone(),
            user_agent: Some(pi_user_agent(VERSION)),
            cancel: context.cancel.clone(),
        }
    }
}

#[async_trait]
impl DynamicCatalog for PersonalizedCatalog {
    fn provider_id(&self) -> &str {
        &self.provider_id
    }

    fn compose(&self, builtin: Vec<Model>) -> Vec<Model> {
        let personal = self.personal.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        if personal.is_empty() { builtin } else { personal.clone() }
    }

    async fn refresh(&self, context: &RefreshContext<'_>) -> anyhow::Result<()> {
        let restored = self.restore(context.stored);
        if !context.publisher.publish(Persist::Unchanged).await {
            return Ok(());
        }
        self.set_personal(restored);
        if !context.allow_network || context.cancel.is_cancelled() || self.is_fresh(context) {
            return Ok(());
        }
        let response =
            request_catalog(&self.client, &self.catalog_base_url, &self.provider_id, None, context.cancel).await?;
        if context.cancel.is_cancelled() {
            return Ok(());
        }
        let status = response.status().as_u16();
        let mut remote = Vec::new();
        if status != 404 && status != 501 {
            anyhow::ensure!(
                response.status().is_success(),
                "Model catalog request failed for {}: {status}",
                self.provider_id
            );
            let catalog = read_catalog(&self.provider_id, response).await?;
            if is_newer(catalog.last_modified, self.local_generated_at) {
                remote = catalog.models;
            }
        }
        let catalog = merge_models(self.builtin.clone(), &remote);
        let personalized = (self.personalize)(catalog, self.list_context(context)).await.map_err(anyhow::Error::msg)?;
        if context.cancel.is_cancelled() {
            return Ok(());
        }
        let entry = ModelsStoreEntry {
            checked_at: Some(pi_ai::now_ms()),
            ..ModelsStoreEntry::with_models(personalized.clone())
        };
        if context.publisher.publish(Persist::Write(entry)).await {
            self.set_personal(personalized);
        }
        Ok(())
    }
}
