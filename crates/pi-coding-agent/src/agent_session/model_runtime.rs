//! The model catalog plus credentials (upstream `ModelRuntime`): which models exist, which
//! providers have usable auth, runtime API keys, and request auth resolution.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use pi_ai::auth::{
    AuthResolver, Credential, CredentialStore, FileCredentialStore, OAuthAuth, OAuthLookup, RuntimeCredentialStore,
};
use pi_ai::{Model, ModelRegistry};

use super::runtime_catalog::{CatalogState, ProviderAuth};
use super::runtime_virtual::RegisteredVirtualModel;
use crate::config::{auth_path, models_path};
use crate::model_resolver::ModelCatalog;

/// Where a model runtime loads models and credentials from.
#[derive(Clone)]
pub struct ModelRuntimeOptions {
    pub models_path: PathBuf,
    /// Credential store in place of `<agent dir>/auth.json`.
    pub credentials: Arc<dyn CredentialStore>,
    /// OAuth providers by id for token refresh; `None` disables OAuth refresh.
    pub oauth: Option<OAuthLookup>,
}

impl ModelRuntimeOptions {
    /// `models.json` and `auth.json` in `agent_dir`, with the built-in OAuth providers.
    pub fn for_agent_dir(agent_dir: &Path) -> Self {
        Self {
            models_path: models_path(agent_dir),
            credentials: Arc::new(FileCredentialStore::new(auth_path(agent_dir))),
            oauth: Some(Arc::new(pi_ai::auth::oauth::builtin_oauth)),
        }
    }
}

pub struct ModelRuntime {
    registry: RwLock<ModelRegistry>,
    resolver: Arc<AuthResolver>,
    runtime_store: Arc<RuntimeCredentialStore>,
    models_path: PathBuf,
    load_error: Option<String>,
    /// Providers with usable auth at the last check.
    pub(super) configured: RwLock<HashSet<String>>,
    pub(super) virtual_models: RwLock<BTreeMap<(String, String), RegisteredVirtualModel>>,
    pub(super) catalogs: RwLock<CatalogState>,
    pub(super) provider_auth: RwLock<HashMap<String, Arc<dyn ProviderAuth>>>,
    /// `pi.registerProvider` configs by provider id, applied over `models.json`.
    pub(super) extension_providers: RwLock<BTreeMap<String, serde_json::Value>>,
    pub(super) extension_oauth: Arc<RwLock<HashMap<String, Arc<dyn OAuthAuth>>>>,
}

pub(super) fn read<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(super) fn write<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl ModelRuntime {
    /// Load `models.json` (errors are kept as [`Self::load_error`]) and find configured providers.
    pub async fn create(options: ModelRuntimeOptions) -> Self {
        let (registry, error) = ModelRegistry::load_lenient(&options.models_path);
        let runtime_store = Arc::new(RuntimeCredentialStore::new(options.credentials));
        let builtin = options.oauth.unwrap_or_else(|| Arc::new(|_: &str| None));
        let extension_oauth: Arc<RwLock<HashMap<String, Arc<dyn OAuthAuth>>>> = Arc::default();
        let registered = extension_oauth.clone();
        let oauth: OAuthLookup =
            Arc::new(move |provider: &str| read(&registered).get(provider).cloned().or_else(|| builtin(provider)));
        let resolver = Arc::new(AuthResolver::new(runtime_store.clone(), oauth));
        resolver.set_provider_configs(registry.auth_configs());
        let runtime = Self {
            registry: RwLock::new(registry),
            resolver,
            runtime_store,
            models_path: options.models_path,
            load_error: error.map(|error| error.to_string()),
            configured: RwLock::default(),
            virtual_models: RwLock::default(),
            catalogs: RwLock::default(),
            provider_auth: RwLock::default(),
            extension_providers: RwLock::default(),
            extension_oauth,
        };
        runtime.refresh_configured_providers().await;
        runtime
    }

    /// The models.json error found at load time, if any.
    pub fn load_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    pub fn models_path(&self) -> &Path {
        &self.models_path
    }

    pub fn registry(&self) -> RwLockReadGuard<'_, ModelRegistry> {
        read(&self.registry)
    }

    pub(super) fn registry_mut(&self) -> RwLockWriteGuard<'_, ModelRegistry> {
        write(&self.registry)
    }

    /// Providers with usable auth at the last check, plus providers of only virtual models.
    pub(super) fn configured_providers(&self) -> HashSet<String> {
        let mut configured = read(&self.configured).clone();
        configured.extend(self.virtual_only_providers());
        configured
    }

    pub fn resolver(&self) -> &Arc<AuthResolver> {
        &self.resolver
    }

    /// The persistent credential store (`auth.json`), without runtime keys.
    pub fn credential_store(&self) -> Arc<dyn CredentialStore> {
        self.runtime_store.inner().clone()
    }

    pub fn get_model(&self, provider: &str, id: &str) -> Option<Model> {
        self.registry().find(provider, id).cloned()
    }

    pub fn all_models(&self) -> Vec<Model> {
        self.registry().models().into_iter().cloned().collect()
    }

    /// Models whose provider has configured auth.
    pub fn available_models(&self) -> Vec<Model> {
        self.registry().available_models().into_iter().cloned().collect()
    }

    /// Whether auth for `provider` was found at the last refresh (no token refresh, no commands).
    pub fn has_configured_auth(&self, provider: &str) -> bool {
        self.registry().has_api_key(provider)
    }

    /// Check auth now, without refreshing OAuth tokens.
    pub async fn check_auth(&self, provider: &str) -> bool {
        if let Some(auth) = self.provider_auth(provider) {
            let stored = self.resolver.store().read(provider).await.ok().flatten();
            return auth.check(stored.as_ref()).await;
        }
        if self.virtual_only_providers().contains(provider) {
            return true;
        }
        matches!(self.resolver.check(provider).await, Ok(Some(_)))
    }

    /// Whether `provider` uses a stored OAuth credential.
    pub async fn is_using_oauth(&self, provider: &str) -> bool {
        matches!(self.resolver.store().read(provider).await, Ok(Some(Credential::OAuth(_))))
    }

    /// Recompute which providers have usable auth.
    pub async fn refresh_configured_providers(&self) {
        let providers = self.registry().providers();
        let mut configured = HashSet::new();
        for provider in providers {
            if self.check_auth(&provider).await {
                configured.insert(provider);
            }
        }
        *write(&self.configured) = configured;
        let configured = self.configured_providers();
        self.registry_mut().set_configured_providers(configured);
    }

    /// A key for this process only (`--api-key`), never written to disk.
    pub fn set_runtime_api_key(&self, provider: &str, key: &str) {
        self.runtime_store.set_runtime_api_key(provider, key);
        write(&self.configured).insert(provider.to_string());
        let configured = self.configured_providers();
        self.registry_mut().set_configured_providers(configured);
    }

    /// The API key a request to `provider` would use now, refreshing OAuth tokens if needed.
    pub async fn api_key_for(&self, provider: &str) -> Option<String> {
        let resolved = self.resolver.resolve(provider, &Default::default()).await.ok().flatten();
        resolved.and_then(|resolved| resolved.auth.api_key)
    }
}

impl ModelCatalog for ModelRuntime {
    fn all_chat_models(&self) -> Vec<Model> {
        self.all_models()
    }

    fn has_configured_auth(&self, provider: &str) -> bool {
        ModelRuntime::has_configured_auth(self, provider)
    }

    fn find_model(&self, provider: &str, id: &str) -> Option<Model> {
        self.get_model(provider, id)
    }
}
