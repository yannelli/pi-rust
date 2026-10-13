//! Dynamic provider catalogs and their refresh: restore from `models-store.json` first, then,
//! with network access and a credential, fetch newer lists (upstream `Models.refresh`).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pi_ai::Model;
use pi_ai::auth::{ApiKeyCredential, AuthOverrides, AuthResolver, Credential};
use tokio_util::sync::CancellationToken;

use super::store::{ModelsStore, ModelsStoreEntry};

/// What a refresh phase does with the provider's persisted entry.
#[derive(Clone, Debug, PartialEq)]
pub enum Persist {
    Unchanged,
    Write(ModelsStoreEntry),
    Delete,
}

/// Generation-checked persistence for one provider refresh.
#[async_trait]
pub trait CatalogPublisher: Send + Sync {
    /// Apply `persist`; false when a newer refresh of the provider or cancellation superseded
    /// this one, in which case the caller must not change its in-memory catalog.
    async fn publish(&self, persist: Persist) -> bool;
}

pub struct RefreshContext<'a> {
    /// The effective credential: the stored one while restoring, the resolved one online.
    pub credential: Option<&'a Credential>,
    /// The persisted entry captured before this phase, without unknown model types.
    pub stored: Option<&'a ModelsStoreEntry>,
    pub allow_network: bool,
    /// Skip freshness checks and fetch now.
    pub force: bool,
    pub cancel: &'a CancellationToken,
    pub publisher: &'a dyn CatalogPublisher,
}

/// Stored and resolved credentials for the network phase.
#[async_trait]
pub trait RefreshCredentials: Send + Sync {
    async fn stored(&self, provider_id: &str) -> anyhow::Result<Option<Credential>>;
    /// The credential to use online, or `None` when the provider is not configured.
    async fn resolve(&self, provider_id: &str, stored: Option<&Credential>) -> anyhow::Result<Option<Credential>>;
}

/// A provider whose models change at runtime.
#[async_trait]
pub trait DynamicCatalog: Send + Sync {
    fn provider_id(&self) -> &str;

    /// The provider's models of every type, given its built-in models.
    fn compose(&self, builtin: Vec<Model>) -> Vec<Model>;

    /// Restore `context.stored` and, when allowed, fetch a newer list. On failure the earlier
    /// list stays.
    async fn refresh(&self, context: &RefreshContext<'_>) -> anyhow::Result<()>;

    async fn resolve_credential(
        &self,
        stored: Option<&Credential>,
        credentials: &dyn RefreshCredentials,
    ) -> anyhow::Result<Option<Credential>> {
        credentials.resolve(self.provider_id(), stored).await
    }
}

/// Credentials from an [`AuthResolver`]: OAuth tokens refreshed as needed, API keys from
/// `auth.json`, `models.json`, or the environment.
pub struct ResolverCredentials {
    resolver: Arc<AuthResolver>,
}

impl ResolverCredentials {
    pub fn new(resolver: Arc<AuthResolver>) -> Self {
        Self { resolver }
    }
}

#[async_trait]
impl RefreshCredentials for ResolverCredentials {
    async fn stored(&self, provider_id: &str) -> anyhow::Result<Option<Credential>> {
        self.resolver.store().read(provider_id).await.map_err(|error| anyhow::anyhow!("{error}"))
    }

    async fn resolve(&self, provider_id: &str, _stored: Option<&Credential>) -> anyhow::Result<Option<Credential>> {
        let resolved = self.resolver.resolve(provider_id, &AuthOverrides::default()).await;
        let resolved = resolved.map_err(|error| anyhow::anyhow!("{error}"))?;
        Ok(resolved
            .map(|resolved| Credential::ApiKey(ApiKeyCredential { key: resolved.auth.api_key, env: resolved.env })))
    }
}

#[derive(Clone, Debug)]
pub struct RefreshOptions {
    pub allow_network: bool,
    /// Only these providers; `None` refreshes all.
    pub providers: Option<Vec<String>>,
    pub force: bool,
    pub cancel: CancellationToken,
}

impl Default for RefreshOptions {
    fn default() -> Self {
        Self { allow_network: true, providers: None, force: false, cancel: CancellationToken::new() }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RefreshResult {
    pub aborted: bool,
    /// Error message by provider id.
    pub errors: BTreeMap<String, String>,
}

/// Runs refreshes against a models store. A newer refresh of a provider supersedes an older one.
pub struct CatalogRefresher {
    store: Arc<dyn ModelsStore>,
    credentials: Arc<dyn RefreshCredentials>,
    generations: Arc<Mutex<HashMap<String, u64>>>,
}

struct StorePublisher {
    store: Arc<dyn ModelsStore>,
    provider_id: String,
    generation: u64,
    generations: Arc<Mutex<HashMap<String, u64>>>,
    cancel: CancellationToken,
}

impl StorePublisher {
    fn current(&self) -> bool {
        let generations = self.generations.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        !self.cancel.is_cancelled() && generations.get(&self.provider_id) == Some(&self.generation)
    }
}

#[async_trait]
impl CatalogPublisher for StorePublisher {
    async fn publish(&self, persist: Persist) -> bool {
        if !self.current() {
            return false;
        }
        let written = match persist {
            Persist::Unchanged => Ok(()),
            Persist::Write(entry) => self.store.write(&self.provider_id, &entry, &self.cancel).await,
            Persist::Delete => self.store.delete(&self.provider_id, &self.cancel).await,
        };
        written.is_ok() && self.current()
    }
}

impl CatalogRefresher {
    pub fn new(store: Arc<dyn ModelsStore>, credentials: Arc<dyn RefreshCredentials>) -> Self {
        Self { store, credentials, generations: Arc::new(Mutex::new(HashMap::new())) }
    }

    fn begin(&self, provider_id: &str) -> u64 {
        let mut generations = self.generations.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let generation = generations.entry(provider_id.into()).or_insert(0);
        *generation += 1;
        *generation
    }

    async fn phase(
        &self,
        catalog: &dyn DynamicCatalog,
        publisher: &StorePublisher,
        credential: Option<&Credential>,
        allow_network: bool,
        force: bool,
    ) -> anyhow::Result<()> {
        let stored = self.store.read(catalog.provider_id(), &publisher.cancel).await?;
        let stored = stored.map(ModelsStoreEntry::with_known_model_types);
        let context = RefreshContext {
            credential,
            stored: stored.as_ref(),
            allow_network,
            force: allow_network && force,
            cancel: &publisher.cancel,
            publisher,
        };
        catalog.refresh(&context).await
    }

    async fn refresh_one(&self, catalog: &dyn DynamicCatalog, options: &RefreshOptions) -> anyhow::Result<()> {
        let provider_id = catalog.provider_id().to_string();
        let publisher = StorePublisher {
            store: self.store.clone(),
            generation: self.begin(&provider_id),
            provider_id: provider_id.clone(),
            generations: self.generations.clone(),
            cancel: options.cancel.child_token(),
        };
        let stored = self.credentials.stored(&provider_id).await;
        let stored_credential = stored.as_ref().ok().and_then(Option::as_ref);
        self.phase(catalog, &publisher, stored_credential, false, false).await?;
        let stored_credential = stored?;
        if !options.allow_network || publisher.cancel.is_cancelled() {
            return Ok(());
        }
        let resolved = catalog.resolve_credential(stored_credential.as_ref(), self.credentials.as_ref()).await?;
        let Some(credential) = resolved else { return Ok(()) };
        self.phase(catalog, &publisher, Some(&credential), true, options.force).await
    }

    /// Refresh `catalogs` (or the selected ones) concurrently and collect per-provider errors.
    pub async fn refresh(&self, catalogs: &[Arc<dyn DynamicCatalog>], options: &RefreshOptions) -> RefreshResult {
        let mut result = RefreshResult { aborted: options.cancel.is_cancelled(), errors: BTreeMap::new() };
        if result.aborted {
            return result;
        }
        let selected = catalogs.iter().filter(|catalog| {
            options.providers.as_ref().is_none_or(|providers| providers.iter().any(|id| id == catalog.provider_id()))
        });
        let runs = selected.map(|catalog| async move {
            let outcome = tokio::select! {
                _ = options.cancel.cancelled() => Ok(()),
                outcome = self.refresh_one(catalog.as_ref(), options) => outcome,
            };
            (catalog.provider_id().to_string(), outcome)
        });
        for (provider_id, outcome) in futures::future::join_all(runs).await {
            if let Err(error) = outcome
                && !options.cancel.is_cancelled()
            {
                result.errors.insert(provider_id, format!("{error:#}"));
            }
        }
        result.aborted = options.cancel.is_cancelled();
        result
    }
}
