//! The model catalog plus credentials (upstream `ModelRuntime`): which models exist, which
//! providers have usable auth, runtime API keys, and request auth resolution.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, RwLockReadGuard};

use pi_ai::auth::{
    AuthResolver, Credential, CredentialStore, FileCredentialStore, OAuthLookup, RuntimeCredentialStore,
};
use pi_ai::{Model, ModelRegistry};

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
}

impl ModelRuntime {
    /// Load `models.json` (errors are kept as [`Self::load_error`]) and find configured providers.
    pub async fn create(options: ModelRuntimeOptions) -> Self {
        let (registry, error) = ModelRegistry::load_lenient(&options.models_path);
        let runtime_store = Arc::new(RuntimeCredentialStore::new(options.credentials));
        let oauth = options.oauth.unwrap_or_else(|| Arc::new(|_: &str| None));
        let resolver = Arc::new(AuthResolver::new(runtime_store.clone(), oauth));
        resolver.set_provider_configs(registry.auth_configs());
        let runtime = Self {
            registry: RwLock::new(registry),
            resolver,
            runtime_store,
            models_path: options.models_path,
            load_error: error.map(|error| error.to_string()),
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
        self.registry.read().unwrap_or_else(|poisoned| poisoned.into_inner())
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
        self.registry.write().unwrap_or_else(|poisoned| poisoned.into_inner()).set_configured_providers(configured);
    }

    /// A key for this process only (`--api-key`), never written to disk.
    pub fn set_runtime_api_key(&self, provider: &str, key: &str) {
        self.runtime_store.set_runtime_api_key(provider, key);
        let mut registry = self.registry.write().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut configured: HashSet<String> =
            registry.providers().into_iter().filter(|name| registry.has_api_key(name)).collect();
        configured.insert(provider.to_string());
        registry.set_configured_providers(configured);
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
