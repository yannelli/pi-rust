//! Dynamic model catalogs in the model runtime: the pi.dev overlay, Radius gateways, and the
//! llama.cpp router restored from `models-store.json` at startup and refreshed in the background
//! (upstream `ModelRuntime.create`, `refresh`, and the llama.cpp built-in extension).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pi_ai::auth::{ApiKeyCredential, Credential, ResolvedAuth};
use pi_ai::models::{ModelRegistry, builtin_models};
use tokio_util::sync::CancellationToken;

use super::model_runtime::{ModelRuntime, read, write};
use crate::catalog::{
    CatalogConfig, CatalogRefresher, DynamicCatalog, MODEL_REFRESH_TIMEOUT, ModelsStore, RefreshOptions, RefreshResult,
    ResolverCredentials, build_registry, builtin_catalogs, refresh_with_timeout,
};
use crate::llama::{LLAMA_PROVIDER_ID, LlamaCatalogSync, LlamaProvider, check_llama_auth, resolve_llama_auth};

mod cached_store;

pub use cached_store::CachedFileModelsStore;

/// Request auth of a provider that `auth.json` and `models.json` cannot describe, such as the
/// llama.cpp router URL (upstream provider `auth.apiKey.resolve`).
#[async_trait]
pub trait ProviderAuth: Send + Sync {
    /// Whether the provider is configured, without side effects.
    async fn check(&self, stored: Option<&Credential>) -> bool;
    async fn resolve(&self, stored: Option<&Credential>) -> Result<Option<ResolvedAuth>, String>;
}

fn api_key(stored: Option<&Credential>) -> Option<&ApiKeyCredential> {
    match stored {
        Some(Credential::ApiKey(credential)) => Some(credential),
        _ => None,
    }
}

fn process_env(name: &str) -> Option<String> {
    pi_ai::env::process_env_value(name)
}

/// The llama.cpp router: a stored `/login llama.cpp` credential or `LLAMA_BASE_URL`.
pub struct LlamaAuth;

#[async_trait]
impl ProviderAuth for LlamaAuth {
    async fn check(&self, stored: Option<&Credential>) -> bool {
        matches!(check_llama_auth(api_key(stored), &process_env), Ok(Some(_)))
    }

    async fn resolve(&self, stored: Option<&Credential>) -> Result<Option<ResolvedAuth>, String> {
        resolve_llama_auth(api_key(stored), &process_env)
    }
}

/// Where dynamic catalogs persist and which pi.dev mirror they use.
#[derive(Clone)]
pub struct CatalogSetup {
    pub store: Arc<dyn ModelsStore>,
    pub config: CatalogConfig,
}

impl CatalogSetup {
    /// `models-store.json` in `agent_dir`; `PI_CATALOG_URL` replaces pi.dev when set.
    pub fn for_agent_dir(agent_dir: &Path) -> Self {
        let store = CachedFileModelsStore::new(crate::catalog::models_store_path(agent_dir));
        Self {
            store: Arc::new(store),
            config: CatalogConfig { catalog_base_url: catalog_base_url(), ..Default::default() },
        }
    }
}

/// `PI_CATALOG_URL`, a mirror of the pi.dev model catalog API.
pub fn catalog_base_url() -> Option<String> {
    process_env("PI_CATALOG_URL").filter(|url| !url.trim().is_empty())
}

#[derive(Default)]
pub(crate) struct CatalogState {
    setup: Option<CatalogSetup>,
    catalogs: Vec<Arc<dyn DynamicCatalog>>,
    refresher: Option<Arc<CatalogRefresher>>,
    llama: Option<Arc<LlamaProvider>>,
}

impl CatalogState {
    fn all(&self) -> Vec<Arc<dyn DynamicCatalog>> {
        let mut catalogs = self.catalogs.clone();
        if let Some(llama) = &self.llama {
            catalogs.push(llama.clone());
        }
        catalogs
    }
}

impl ModelRuntime {
    /// Compose the catalog from the bundled models, the pi.dev overlay, and Radius gateways, and
    /// restore their last lists from the store without network access.
    pub async fn enable_dynamic_catalogs(&self, setup: CatalogSetup) {
        let mut config = setup.config.clone();
        if config.models_json.is_none() {
            config.models_json = std::fs::read_to_string(self.models_path()).ok();
        }
        let mut catalogs = builtin_catalogs(builtin_models(), &config);
        catalogs.retain(|catalog| catalog.provider_id() != LLAMA_PROVIDER_ID);
        let credentials = Arc::new(ResolverCredentials::new(self.resolver().clone()));
        let refresher = Arc::new(CatalogRefresher::new(setup.store.clone(), credentials));
        {
            let mut state = write(&self.catalogs);
            state.setup = Some(setup);
            state.catalogs = catalogs;
            state.refresher = Some(refresher);
        }
        self.refresh_catalogs(false, None, false).await;
    }

    /// Whether dynamic catalogs are on.
    pub fn has_dynamic_catalogs(&self) -> bool {
        read(&self.catalogs).refresher.is_some()
    }

    /// Add or remove the llama.cpp provider (`builtin:llama.cpp`); added, it restores its models.
    pub async fn set_llama_enabled(&self, enabled: bool) {
        let changed = {
            let mut state = write(&self.catalogs);
            match (enabled, state.llama.is_some()) {
                (true, false) => state.llama = Some(Arc::new(LlamaProvider::new())),
                (false, true) => state.llama = None,
                _ => return,
            }
            true
        };
        {
            let mut auth = write(&self.provider_auth);
            if enabled {
                auth.insert(LLAMA_PROVIDER_ID.into(), Arc::new(LlamaAuth));
            } else {
                auth.remove(LLAMA_PROVIDER_ID);
            }
        }
        if changed && enabled {
            self.refresh_catalogs(false, Some(vec![LLAMA_PROVIDER_ID.into()]), false).await;
        } else {
            self.rebuild_registry();
            self.refresh_configured_providers().await;
        }
    }

    /// The llama.cpp provider when `builtin:llama.cpp` is on.
    pub fn llama_provider(&self) -> Option<Arc<LlamaProvider>> {
        read(&self.catalogs).llama.clone()
    }

    pub(super) fn provider_auth(&self, provider: &str) -> Option<Arc<dyn ProviderAuth>> {
        read(&self.provider_auth).get(provider).cloned()
    }

    /// Request auth from a [`ProviderAuth`]; `None` when the provider has none.
    pub async fn resolve_provider_auth(&self, provider: &str) -> Option<Result<Option<ResolvedAuth>, String>> {
        let auth = self.provider_auth(provider)?;
        let stored = self.resolver().store().read(provider).await.ok().flatten();
        Some(auth.resolve(stored.as_ref()).await)
    }

    /// Bundled models, catalogs, and `models.json`, before extension providers.
    pub(super) fn base_registry(&self) -> ModelRegistry {
        let catalogs = read(&self.catalogs).all();
        let (registry, _) = if catalogs.is_empty() {
            ModelRegistry::load_lenient(self.models_path())
        } else {
            build_registry(builtin_models(), &catalogs, Some(self.models_path()))
        };
        registry
    }

    /// Recompose the registry: bundled models, catalogs, `models.json`, extension providers,
    /// `modelOverrides`, then virtual models.
    pub(super) fn rebuild_registry(&self) {
        let mut registry = self.base_registry();
        self.apply_extension_providers(&mut registry);
        self.resolver().set_provider_configs(registry.auth_configs());
        *self.registry_mut() = registry;
        self.apply_virtual_models();
    }

    /// Refresh the catalogs (or `providers`), then recompose the registry and recheck auth.
    /// Without network access this restores the stored lists.
    pub async fn refresh_catalogs(
        &self,
        allow_network: bool,
        providers: Option<Vec<String>>,
        force: bool,
    ) -> RefreshResult {
        let options = RefreshOptions { allow_network, providers, force, cancel: CancellationToken::new() };
        self.refresh_catalogs_with(options).await
    }

    pub async fn refresh_catalogs_with(&self, options: RefreshOptions) -> RefreshResult {
        let (refresher, catalogs) = {
            let state = read(&self.catalogs);
            (state.refresher.clone(), state.all())
        };
        let mut result = match refresher {
            Some(refresher) => refresher.refresh(&catalogs, &options).await,
            None => RefreshResult::default(),
        };
        if !result.aborted {
            self.refresh_extension_providers(&options, &mut result).await;
        }
        self.rebuild_registry();
        self.refresh_configured_providers().await;
        result
    }

    /// Refresh every catalog from the network with a time limit (`PI_OFFLINE` skips it), as
    /// interactive and RPC modes do after startup. `None` when there is nothing to refresh.
    pub fn refresh_in_background(self: &Arc<Self>) -> Option<tokio::task::JoinHandle<RefreshResult>> {
        self.refresh_in_background_within(MODEL_REFRESH_TIMEOUT)
    }

    pub fn refresh_in_background_within(
        self: &Arc<Self>,
        timeout: Duration,
    ) -> Option<tokio::task::JoinHandle<RefreshResult>> {
        if crate::config::offline() {
            return None;
        }
        let (refresher, catalogs) = {
            let state = read(&self.catalogs);
            (state.refresher.clone(), state.all())
        };
        if refresher.is_none() && !self.has_extension_refresh() {
            return None;
        }
        let runtime = self.clone();
        Some(tokio::spawn(async move {
            let cancel = CancellationToken::new();
            let mut result = match &refresher {
                Some(refresher) => refresh_with_timeout(refresher, &catalogs, true, false, timeout, &cancel).await,
                None => RefreshResult::default(),
            };
            if !result.aborted {
                let options = RefreshOptions { allow_network: true, providers: None, force: false, cancel };
                let refresh = runtime.refresh_extension_providers(&options, &mut result);
                if tokio::time::timeout(timeout, refresh).await.is_err() {
                    result.aborted = true;
                }
            }
            runtime.rebuild_registry();
            runtime.refresh_configured_providers().await;
            result
        }))
    }
}

/// `/llama` refreshes the llama.cpp models with network access, even offline, as it already
/// reached the router.
#[async_trait]
impl LlamaCatalogSync for ModelRuntime {
    async fn refresh_llama_models(&self) -> Result<(), String> {
        let result = self.refresh_catalogs(true, Some(vec![LLAMA_PROVIDER_ID.into()]), true).await;
        match result.errors.get(LLAMA_PROVIDER_ID) {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }
}
