//! Native providers from extensions (upstream `Provider` objects of `pi.registerProvider(provider)`
//! and `ModelRuntime.registerNativeProvider`). A native provider replaces the built-in provider and
//! the `pi.registerProvider(name, config)` registration of its id: the extension supplies its models,
//! auth, request streams, image and classifier requests, and catalog refresh.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use pi_ai::auth::{ApiKeyCredential, Credential, OAuthAuth, refresh_stored_oauth};
use pi_ai::classify::{ClassifierContext, ClassifierOptions, ClassifierResult};
use pi_ai::images::{AssistantImages, ImagesContext, ImagesOptions};
use pi_ai::{AssistantMessageEventStream, Context, Model, ModelRegistry, StreamOptions};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::model_runtime::{ModelRuntime, read, write};
use super::runtime_catalog::ProviderAuth;
use super::runtime_extensions::model_kind;
use crate::catalog::store::KNOWN_MODEL_TYPES;
use crate::catalog::{FileModelsStore, ModelsStore, RefreshOptions, RefreshResult, models_store_path};

/// What `refreshModels(context)` receives (upstream `RefreshModelsContext`). Its `publish` reaches
/// [`ModelRuntime::publish_native_models`] with `generation`.
#[derive(Clone, Debug, Default)]
pub struct NativeRefreshContext {
    pub credential: Option<Credential>,
    /// The stored catalog entry without models of unknown types.
    pub stored: Option<Value>,
    pub allow_network: bool,
    pub force: Option<bool>,
    pub base_url: Option<String>,
    pub generation: u64,
}

/// The functions of a native provider, implemented by the extension host.
#[async_trait]
pub trait NativeProvider: Send + Sync {
    /// `streamSimple(model, context, options)`; `options` carry the resolved auth.
    fn stream(&self, model: &Model, context: &Context, options: StreamOptions) -> AssistantMessageEventStream;
    /// `refreshModels(context)`: the provider's models of every type when it finished.
    async fn refresh_models(
        &self,
        context: NativeRefreshContext,
        cancel: CancellationToken,
    ) -> Result<Vec<Value>, String>;
    /// `filterAllModels` or `filterModels` with the stored credential: `(type, id)` of the models kept.
    async fn filter_models(&self, credential: Option<Credential>) -> Result<Vec<(String, String)>, String>;
    async fn generate_images(&self, model: &Model, context: &ImagesContext, options: ImagesOptions) -> AssistantImages;
    async fn classify(
        &self,
        model: &Model,
        context: &ClassifierContext,
        options: ClassifierOptions,
    ) -> ClassifierResult;
}

/// What a native provider declares besides its functions.
#[derive(Clone, Debug, Default)]
pub struct NativeProviderInfo {
    pub name: String,
    pub base_url: Option<String>,
    pub refresh_models: bool,
    /// `refreshModelsAuth: "configured"`: the network phase runs without a credential.
    pub refresh_with_configured_auth: bool,
    pub filter_models: bool,
}

/// One `registerNativeProvider` call.
pub struct NativeRegistration {
    pub provider: Arc<dyn NativeProvider>,
    pub info: NativeProviderInfo,
    /// `getAllModels()`, else `getModels()`.
    pub models: Vec<Value>,
    pub auth: Arc<dyn ProviderAuth>,
    pub oauth: Option<Arc<dyn OAuthAuth>>,
}

pub(crate) struct NativeEntry {
    provider: Arc<dyn NativeProvider>,
    info: NativeProviderInfo,
    models: Vec<Model>,
    /// Changes with each registration and refresh; publications of older ones are dropped.
    generation: u64,
    refresh: CancellationToken,
    /// `(type, id)` of the models `filterModels` kept at the last availability check.
    available: Option<HashSet<(String, String)>>,
}

static GENERATIONS: AtomicU64 = AtomicU64::new(1);

/// A model of a native provider. Null fields count as absent, and a missing `provider`,
/// `baseUrl`, `name`, or `api` is filled in; models that Rust cannot read are left out.
fn native_model(provider: &str, base_url: Option<&str>, value: &Value) -> Option<Model> {
    let mut object = value.as_object()?.clone();
    object.retain(|_, field| !field.is_null());
    let id = object.get("id")?.as_str()?.to_string();
    object.entry("provider").or_insert_with(|| json!(provider));
    object.entry("baseUrl").or_insert_with(|| json!(base_url.unwrap_or_default()));
    object.entry("name").or_insert_with(|| json!(id));
    object.entry("api").or_insert_with(|| json!(""));
    if object.get("type").and_then(Value::as_str) == Some("chat") {
        object.remove("type");
    }
    serde_json::from_value(Value::Object(object)).ok()
}

fn native_models(provider: &str, base_url: Option<&str>, models: &[Value]) -> Vec<Model> {
    models.iter().filter_map(|model| native_model(provider, base_url, model)).collect()
}

/// A stored entry without models of unknown types (upstream `withKnownModelTypes`).
fn with_known_model_types(mut entry: Value) -> Value {
    if let Some(models) = entry.get_mut("models").and_then(Value::as_array_mut) {
        models.retain(|model| KNOWN_MODEL_TYPES.contains(&model["type"].as_str().unwrap_or("chat")));
    }
    entry
}

impl ModelRuntime {
    /// Register or replace native provider `id`. Returns the generation of the registration.
    pub fn register_native_provider(&self, id: &str, registration: NativeRegistration) -> u64 {
        let NativeRegistration { provider, info, models, auth, oauth } = registration;
        write(&self.extension_providers).remove(id);
        write(&self.extension_hooks).remove(id);
        let generation = GENERATIONS.fetch_add(1, Ordering::Relaxed);
        let models = native_models(id, info.base_url.as_deref(), &models);
        let entry =
            NativeEntry { provider, info, models, generation, refresh: CancellationToken::new(), available: None };
        if let Some(previous) = write(&self.native_providers).insert(id.into(), entry) {
            previous.refresh.cancel();
        }
        self.register_provider_auth(id, Some(auth));
        write(&self.extension_oauth).insert(id.into(), oauth);
        self.rebuild_keeping_configured();
        generation
    }

    /// Remove native provider `id`; false when it has none.
    pub fn unregister_native_provider(&self, id: &str) -> bool {
        let Some(previous) = write(&self.native_providers).remove(id) else { return false };
        previous.refresh.cancel();
        self.register_provider_auth(id, None);
        write(&self.extension_oauth).remove(id);
        self.rebuild_keeping_configured();
        true
    }

    pub fn is_native_provider(&self, id: &str) -> bool {
        read(&self.native_providers).contains_key(id)
    }

    pub fn native_provider(&self, id: &str) -> Option<Arc<dyn NativeProvider>> {
        read(&self.native_providers).get(id).map(|entry| entry.provider.clone())
    }

    fn native_generation(&self, id: &str) -> Option<u64> {
        read(&self.native_providers).get(id).map(|entry| entry.generation)
    }

    /// Replace the models of native provider `id` when `generation` is still its current one.
    pub fn set_native_models(&self, id: &str, generation: u64, models: &[Value]) -> bool {
        {
            let mut natives = write(&self.native_providers);
            let Some(entry) = natives.get_mut(id).filter(|entry| entry.generation == generation) else {
                return false;
            };
            entry.models = native_models(id, entry.info.base_url.as_deref(), models);
        }
        self.rebuild_keeping_configured();
        true
    }

    /// Recompose the registry, keeping the providers known to have auth until the next check.
    fn rebuild_keeping_configured(&self) {
        self.rebuild_registry();
        let configured = self.configured_providers();
        self.registry_mut().set_configured_providers(configured);
    }

    /// The models of native providers replace the providers of the same id, then
    /// `modelOverrides` apply.
    pub(super) fn apply_native_providers(&self, registry: &mut ModelRegistry) {
        for (id, entry) in read(&self.native_providers).iter() {
            registry.replace_provider_models(id, entry.models.clone());
            if !entry.info.name.is_empty() {
                registry.set_provider_name(id, &entry.info.name);
            }
            let _ = registry.apply_model_overrides(id);
        }
    }

    /// Upstream `publish({ persist })` of a refresh with `generation`: `Some(None)` deletes the
    /// stored catalog, `Some(Some(entry))` writes it. False when a later registration or refresh
    /// superseded that one.
    pub async fn publish_native_models(
        &self,
        id: &str,
        generation: u64,
        persist: Option<Option<Value>>,
        cancel: &CancellationToken,
    ) -> Result<bool, String> {
        let current = || self.native_generation(id) == Some(generation) && !cancel.is_cancelled();
        if !current() {
            return Ok(false);
        }
        let store = self.native_models_store();
        let written = match persist {
            None => Ok(()),
            Some(None) => store.delete(id, cancel).await,
            Some(Some(entry)) => store.write_value(id, &entry, cancel).await,
        };
        written.map_err(|error| format!("{error:#}"))?;
        Ok(current())
    }

    fn native_models_store(&self) -> Arc<dyn ModelsStore> {
        read(&self.catalogs).models_store().unwrap_or_else(|| {
            let dir = self.models_path().parent().unwrap_or_else(|| std::path::Path::new("."));
            Arc::new(FileModelsStore::new(models_store_path(dir)))
        })
    }

    /// Whether native providers can refresh their models.
    pub(super) fn has_native_refresh(&self) -> bool {
        read(&self.native_providers).values().any(|entry| entry.info.refresh_models)
    }

    /// Upstream `Models.refresh` for native providers: each restores its stored catalog offline,
    /// then fetches with its credential when network access is allowed.
    pub(super) async fn refresh_native_providers(&self, options: &RefreshOptions, result: &mut RefreshResult) {
        let ids: Vec<String> = read(&self.native_providers)
            .iter()
            .filter(|(id, entry)| {
                entry.info.refresh_models && options.providers.as_ref().is_none_or(|wanted| wanted.contains(id))
            })
            .map(|(id, _)| id.clone())
            .collect();
        let refreshes = ids.into_iter().map(|id| async move {
            let outcome = self.refresh_native(&id, options).await;
            (id, outcome)
        });
        for (id, outcome) in futures::future::join_all(refreshes).await {
            if let Err(error) = outcome {
                result.errors.insert(id, error);
            }
        }
        if options.cancel.is_cancelled() {
            result.aborted = true;
        }
    }

    /// Supersede the refresh of `id` in progress and start one; `None` when `id` is not native.
    fn begin_native_refresh(
        &self,
        id: &str,
        caller: &CancellationToken,
    ) -> Option<(Arc<dyn NativeProvider>, NativeProviderInfo, u64, CancellationToken)> {
        let mut natives = write(&self.native_providers);
        let entry = natives.get_mut(id)?;
        entry.refresh.cancel();
        entry.refresh = caller.child_token();
        entry.generation = GENERATIONS.fetch_add(1, Ordering::Relaxed);
        Some((entry.provider.clone(), entry.info.clone(), entry.generation, entry.refresh.clone()))
    }

    async fn refresh_native(&self, id: &str, options: &RefreshOptions) -> Result<(), String> {
        let Some((provider, info, generation, cancel)) = self.begin_native_refresh(id, &options.cancel) else {
            return Ok(());
        };
        let phases = async {
            let (stored, read_error) = match self.resolver().store().read(id).await {
                Ok(stored) => (stored, None),
                Err(error) => (None, Some(format!("Credential store read failed for {id}: {error}"))),
            };
            let phase = |credential, allow_network, force| {
                let context = NativeRefreshContext {
                    credential,
                    stored: None,
                    allow_network,
                    force,
                    base_url: info.base_url.clone(),
                    generation,
                };
                self.native_refresh_phase(id, &provider, context, &cancel)
            };
            // Restore the cached catalog before auth resolution or network access.
            phase(stored.clone(), false, None).await?;
            if let Some(error) = read_error {
                return Err(error);
            }
            if !options.allow_network || cancel.is_cancelled() {
                return Ok(());
            }
            let Some(credential) = self.native_refresh_credential(id, &info, stored).await? else { return Ok(()) };
            phase(credential, true, Some(options.force)).await
        };
        let outcome: Result<(), String> = phases.await;
        // A cancelled or superseded refresh reports no error.
        if cancel.is_cancelled() { Ok(()) } else { outcome }
    }

    async fn native_refresh_phase(
        &self,
        id: &str,
        provider: &Arc<dyn NativeProvider>,
        mut context: NativeRefreshContext,
        cancel: &CancellationToken,
    ) -> Result<(), String> {
        let store = self.native_models_store();
        let stored = store.read_value(id, cancel).await.map_err(|error| format!("{error:#}"))?;
        context.stored = stored.map(with_known_model_types);
        let generation = context.generation;
        let models = provider.refresh_models(context, cancel.clone()).await?;
        self.set_native_models(id, generation, &models);
        Ok(())
    }

    /// The credential of the network phase (upstream `resolveNetworkRefresh`): `Some(None)` for
    /// `refreshModelsAuth: "configured"`, `None` when the provider is not configured. An expired
    /// OAuth token is refreshed under the store lock first.
    async fn native_refresh_credential(
        &self,
        id: &str,
        info: &NativeProviderInfo,
        stored: Option<Credential>,
    ) -> Result<Option<Option<Credential>>, String> {
        let auth = self.provider_auth(id);
        if info.refresh_with_configured_auth {
            let configured = match &auth {
                Some(auth) => auth.check(stored.as_ref()).await,
                None => false,
            };
            return Ok(configured.then_some(None));
        }
        if let Some(Credential::OAuth(credential)) = &stored {
            let Some(oauth) = self.resolver().oauth(id) else { return Ok(None) };
            if !credential.expires_within(pi_ai::now_ms(), 0) {
                return Ok(Some(stored));
            }
            let refreshed = refresh_stored_oauth(self.resolver().store(), id, oauth, 0).await;
            return Ok(refreshed.map_err(|error| error.to_string())?.map(|post| Some(Credential::OAuth(post))));
        }
        let Some(auth) = auth else { return Ok(None) };
        let credential = stored.as_ref().filter(|stored| matches!(stored, Credential::ApiKey(_)));
        let resolved = auth.resolve(credential).await?;
        Ok(resolved.map(|resolved| {
            Some(Credential::ApiKey(ApiKeyCredential { key: resolved.auth.api_key, env: resolved.env }))
        }))
    }

    /// Run `filterModels` of the configured native providers that have one.
    pub(super) async fn refresh_native_availability(&self, configured: &HashSet<String>) {
        let providers: Vec<(String, Arc<dyn NativeProvider>)> = read(&self.native_providers)
            .iter()
            .filter(|(id, entry)| entry.info.filter_models && configured.contains(*id))
            .map(|(id, entry)| (id.clone(), entry.provider.clone()))
            .collect();
        for (id, provider) in providers {
            let credential = self.resolver().store().read(&id).await.ok().flatten();
            let kept = provider.filter_models(credential).await.ok().map(|kept| kept.into_iter().collect());
            if let Some(entry) = write(&self.native_providers).get_mut(&id) {
                entry.available = kept;
            }
        }
    }

    /// Whether `model` has configured auth and its native provider's `filterModels` keeps it.
    pub fn is_model_available(&self, model: &Model) -> bool {
        if !self.has_configured_auth(&model.provider) {
            return false;
        }
        let natives = read(&self.native_providers);
        let kept = natives.get(&model.provider).and_then(|entry| entry.available.as_ref());
        kept.is_none_or(|kept| kept.contains(&(model_kind(model).to_string(), model.id.clone())))
    }
}
