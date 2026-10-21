//! Dynamic model catalogs: the pi.dev overlay of bundled providers, account model lists, Radius
//! gateway catalogs, llama.cpp, the `models-store.json` offline cache, and `pi update --models`.

pub mod personalized;
pub mod radius;
pub mod refresh;
pub mod remote;
pub mod store;
pub mod update;

#[cfg(test)]
pub(crate) mod tests;

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use pi_ai::Model;
use pi_ai::models::{ModelRegistry, ModelsFileError};

pub use personalized::{Personalize, PersonalizedCatalog, account_model_list};
pub use radius::{RADIUS_PROVIDER_ID, RadiusCatalog, RadiusProviderConfig, radius_providers_in_models_json};
pub use refresh::{
    CatalogPublisher, CatalogRefresher, DynamicCatalog, Persist, RefreshContext, RefreshCredentials, RefreshOptions,
    RefreshResult, ResolverCredentials,
};
pub use remote::{DEFAULT_CATALOG_BASE_URL, REMOTE_CATALOG_REFRESH_INTERVAL_MS, RemoteCatalogOverlay, merge_models};
pub use store::{FileModelsStore, InMemoryModelsStore, ModelsStore, ModelsStoreEntry, models_store_path};
pub use update::{
    CatalogConfig, MODEL_REFRESH_TIMEOUT, builtin_catalogs, refresh_model_catalogs, refresh_with_timeout,
    update_models_outcome,
};

/// Bundled models with each catalog applied to its provider. Providers keep their bundled
/// order; catalogs of providers without bundled models follow at the end.
pub fn compose_catalog(builtin: &[Model], catalogs: &[Arc<dyn DynamicCatalog>]) -> Vec<Model> {
    let mut composed = Vec::with_capacity(builtin.len());
    let mut done: HashSet<&str> = HashSet::new();
    for model in builtin {
        let provider = model.provider.as_str();
        if done.contains(provider) {
            continue;
        }
        match catalogs.iter().find(|catalog| catalog.provider_id() == provider) {
            Some(catalog) => {
                let own: Vec<Model> = builtin.iter().filter(|model| model.provider == provider).cloned().collect();
                composed.extend(catalog.compose(own));
                done.insert(provider);
            }
            None => composed.push(model.clone()),
        }
    }
    for catalog in catalogs {
        if done.insert(catalog.provider_id()) {
            composed.extend(catalog.compose(Vec::new()));
        }
    }
    composed
}

/// A registry of [`compose_catalog`] with `models.json` applied. An unreadable or invalid file
/// leaves the composed catalog and is returned as the error.
pub fn build_registry(
    builtin: &[Model],
    catalogs: &[Arc<dyn DynamicCatalog>],
    models_json: Option<&Path>,
) -> (ModelRegistry, Option<ModelsFileError>) {
    let mut registry = ModelRegistry::default();
    for model in compose_catalog(builtin, catalogs) {
        registry.upsert(model);
    }
    let Some(path) = models_json else { return (registry, None) };
    let mut applied = registry.clone();
    match applied.apply_models_file(path) {
        Ok(()) => (applied, None),
        Err(error) => (registry, Some(error)),
    }
}
