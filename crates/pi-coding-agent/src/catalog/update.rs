//! The built-in catalog set and `pi update --models` (upstream `refreshModelCatalogs` in
//! `package-manager-cli.ts` and the provider wiring in `core/model-runtime.ts`).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use pi_ai::Model;
use pi_ai::auth::{AuthResolver, FileCredentialStore};
use pi_ai::models::provider_base_urls;
use tokio_util::sync::CancellationToken;

use super::personalized::{PersonalizedCatalog, account_model_list};
use super::radius::{RADIUS_PROVIDER_ID, RadiusCatalog, radius_providers_in_models_json};
use super::refresh::{CatalogRefresher, DynamicCatalog, RefreshOptions, RefreshResult, ResolverCredentials};
use super::remote::RemoteCatalogOverlay;
use super::store::{FileModelsStore, models_store_path};
use crate::llama::LlamaProvider;

/// Time limit of startup refreshes and `pi update --models`.
pub const MODEL_REFRESH_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, Default)]
pub struct CatalogConfig {
    /// pi.dev by default.
    pub catalog_base_url: Option<String>,
    /// Generation time of the bundled catalog; an older remote overlay is ignored.
    pub local_generated_at: Option<i64>,
    /// `models.json` text, for its Radius providers and provider base URLs.
    pub models_json: Option<String>,
    /// HTTP client of the pi.dev overlays; the shared client by default.
    pub client: Option<reqwest::Client>,
    /// HTTP client of account model lists, which must not follow redirects; by default
    /// `pi_ai::model_lists::list_client`.
    pub list_client: Option<reqwest::Client>,
}

/// The pi.dev overlay for every bundled provider except Radius, narrowed to the account's models
/// where the provider lists them, the Radius gateway catalogs (built-in and `models.json`), and
/// the llama.cpp router.
pub fn builtin_catalogs(builtin: &[Model], config: &CatalogConfig) -> Vec<Arc<dyn DynamicCatalog>> {
    let mut providers: Vec<&str> = Vec::new();
    for model in builtin {
        if !providers.contains(&model.provider.as_str()) {
            providers.push(&model.provider);
        }
    }
    let mut catalogs: Vec<Arc<dyn DynamicCatalog>> = Vec::new();
    let custom_radius = config.models_json.as_deref().map(radius_providers_in_models_json).unwrap_or_default();
    let base_urls = config.models_json.as_deref().map(provider_base_urls).unwrap_or_default();
    for provider in providers {
        if custom_radius.iter().any(|radius| radius.id == provider) {
            continue;
        }
        if provider == RADIUS_PROVIDER_ID {
            catalogs.push(Arc::new(RadiusCatalog::new(RADIUS_PROVIDER_ID, None)));
            continue;
        }
        if let Some((personalize, interval)) = account_model_list(provider) {
            let own = builtin.iter().filter(|model| model.provider == provider).cloned().collect();
            let mut catalog = PersonalizedCatalog::new(provider, own, personalize)
                .with_catalog(config.catalog_base_url.as_deref(), config.local_generated_at)
                .with_provider_base_url(base_urls.get(provider).cloned())
                .with_interval(interval);
            if let Some(client) = &config.client {
                catalog = catalog.with_client(client.clone());
            }
            if let Some(client) = &config.list_client {
                catalog = catalog.with_list_client(client.clone());
            }
            catalogs.push(Arc::new(catalog));
            continue;
        }
        let mut overlay =
            RemoteCatalogOverlay::new(provider, config.catalog_base_url.as_deref(), config.local_generated_at);
        if let Some(client) = &config.client {
            overlay = overlay.with_client(client.clone());
        }
        catalogs.push(Arc::new(overlay));
    }
    for radius in custom_radius {
        catalogs.push(Arc::new(RadiusCatalog::new(radius.id, Some(&radius.gateway))));
    }
    if !catalogs.iter().any(|catalog| catalog.provider_id() == crate::llama::LLAMA_PROVIDER_ID) {
        catalogs.push(Arc::new(LlamaProvider::new()));
    }
    catalogs
}

/// Run `refresher` with a time limit; the result is `aborted` when the limit or `cancel` ends it.
pub async fn refresh_with_timeout(
    refresher: &CatalogRefresher,
    catalogs: &[Arc<dyn DynamicCatalog>],
    allow_network: bool,
    force: bool,
    timeout: Duration,
    cancel: &CancellationToken,
) -> RefreshResult {
    let token = cancel.child_token();
    let timer = token.clone();
    let deadline = tokio::spawn(async move {
        tokio::time::sleep(timeout).await;
        timer.cancel();
    });
    let options = RefreshOptions { allow_network, providers: None, force, cancel: token };
    let result = refresher.refresh(catalogs, &options).await;
    deadline.abort();
    result
}

/// Turn a forced refresh result into the `pi update --models` outcome.
pub fn update_models_outcome(result: &RefreshResult) -> Result<String, String> {
    if result.aborted {
        return Err("Model catalog refresh timed out.".into());
    }
    if !result.errors.is_empty() {
        let details: Vec<String> =
            result.errors.iter().map(|(provider, error)| format!("{provider}: {error}")).collect();
        return Err(format!("Could not refresh model catalogs: {}", details.join("; ")));
    }
    Ok("Model catalogs refreshed".into())
}

/// `pi update --models`: force-refresh every configured catalog into `models-store.json`.
/// Runs even in offline mode, as the user asked for it.
pub async fn refresh_model_catalogs(agent_dir: &Path, config: &CatalogConfig) -> Result<String, String> {
    let models_json_path = crate::config::models_path(agent_dir);
    let (registry, _) = pi_ai::models::ModelRegistry::load_lenient(&models_json_path);
    let credential_store = Arc::new(FileCredentialStore::new(crate::config::auth_path(agent_dir)));
    let resolver = AuthResolver::new(credential_store, Arc::new(pi_ai::auth::oauth::builtin_oauth));
    resolver.set_provider_configs(registry.auth_configs());
    let refresher = CatalogRefresher::new(
        Arc::new(FileModelsStore::new(models_store_path(agent_dir))),
        Arc::new(ResolverCredentials::new(Arc::new(resolver))),
    );
    let mut config = config.clone();
    if config.models_json.is_none() {
        config.models_json = std::fs::read_to_string(&models_json_path).ok();
    }
    let catalogs = builtin_catalogs(pi_ai::models::builtin_models(), &config);
    let cancel = CancellationToken::new();
    let result = refresh_with_timeout(&refresher, &catalogs, true, true, MODEL_REFRESH_TIMEOUT, &cancel).await;
    update_models_outcome(&result)
}
