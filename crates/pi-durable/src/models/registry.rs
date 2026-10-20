//! [`Models`] over `pi_ai::ModelRegistry`, an `AuthResolver`, and the global API provider registry. Port of the
//! request path of upstream `ModelsImpl`: auth is resolved per request (`applyAuth`), then the provider streams.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};

use futures::StreamExt;
use futures::future::BoxFuture;
use pi_ai::auth::{AuthOverrides, AuthResolver, InMemoryCredentialStore, ResolvedAuth};
use pi_ai::{
    AssistantMessageEventStream, Context, DeferredHandle, Model, ModelRegistry, RequestOptions, StreamOptions,
    event_channel,
};
use pi_chord::AbortSignal;
use tokio_util::sync::CancellationToken;

use super::{DeferredCancelOptions, DeferredFetchOptions, Models, SimpleStreamOptions, setup_error_stream};
use crate::errors::{DurableError, DurableResult};

/// Upstream `createModels()`: an empty collection that providers join through [`RegistryModels::set_provider`]
/// or [`RegistryModels::set_registry`]. Auth comes from the process environment.
pub fn create_models() -> RegistryModels {
    RegistryModels::new(ModelRegistry::default(), Arc::new(default_resolver()))
}

fn default_resolver() -> AuthResolver {
    AuthResolver::without_oauth(Arc::new(InMemoryCredentialStore::new(BTreeMap::new())))
}

/// Models from a `ModelRegistry`, streamed through the provider registered for each model's API. Providers set
/// with [`Self::set_provider`] serve their provider id instead, as upstream `setProvider` does.
pub struct RegistryModels {
    registry: RwLock<ModelRegistry>,
    resolver: Arc<AuthResolver>,
    providers: RwLock<HashMap<String, Arc<dyn Models>>>,
}

impl RegistryModels {
    /// Models of `registry`; `resolver` resolves request auth and receives the registry's `models.json` settings.
    pub fn new(registry: ModelRegistry, resolver: Arc<AuthResolver>) -> Self {
        resolver.set_provider_configs(registry.auth_configs());
        Self { registry: RwLock::new(registry), resolver, providers: RwLock::default() }
    }

    /// The bundled catalog with auth from the process environment, for hosts such as `pi experimental`.
    pub fn builtin() -> Self {
        Self::new(ModelRegistry::builtin(), Arc::new(default_resolver()))
    }

    /// Replace the catalog, for example after `models.json` changed.
    pub fn set_registry(&self, registry: ModelRegistry) {
        self.resolver.set_provider_configs(registry.auth_configs());
        *self.registry.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = registry;
    }

    /// Route requests for models of `provider_id` to `provider`, replacing an earlier one.
    pub fn set_provider(&self, provider_id: impl Into<String>, provider: Arc<dyn Models>) {
        self.providers.write().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(provider_id.into(), provider);
    }

    /// Remove a provider set with [`Self::set_provider`].
    pub fn remove_provider(&self, provider_id: &str) -> Option<Arc<dyn Models>> {
        self.providers.write().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(provider_id)
    }

    pub fn resolver(&self) -> &Arc<AuthResolver> {
        &self.resolver
    }

    fn provider(&self, provider_id: &str) -> Option<Arc<dyn Models>> {
        self.providers.read().unwrap_or_else(|poisoned| poisoned.into_inner()).get(provider_id).cloned()
    }

    fn knows_provider(&self, provider_id: &str) -> bool {
        self.registry.read().unwrap_or_else(|poisoned| poisoned.into_inner()).has_provider(provider_id)
    }
}

/// Auth for one request: the model with its resolved base URL, and the merged key, headers, and env.
struct AppliedAuth {
    model: Model,
    api_key: Option<String>,
    headers: BTreeMap<String, String>,
    env: BTreeMap<String, String>,
}

/// Upstream `applyAuth`: explicit request values win per field over the resolved auth.
async fn apply_auth(
    resolver: &AuthResolver,
    model: &Model,
    api_key: Option<String>,
    env: &BTreeMap<String, String>,
    headers: &BTreeMap<String, String>,
) -> Result<AppliedAuth, String> {
    let overrides =
        AuthOverrides { api_key: api_key.clone(), env: (!env.is_empty()).then(|| env.clone()), ..Default::default() };
    let resolved: Option<ResolvedAuth> =
        resolver.resolve_for_model(&model.provider, &model.id, &overrides).await.map_err(|error| error.to_string())?;
    let Some(resolved) = resolved else { return Err(format!("Provider is not configured: {}", model.provider)) };
    let mut request_model = model.clone();
    if let Some(base_url) = &resolved.auth.base_url {
        request_model.base_url = base_url.clone();
    }
    let mut merged_headers = resolved.auth.headers.clone();
    merged_headers.extend(headers.iter().map(|(name, value)| (name.clone(), value.clone())));
    let mut merged_env = resolved.env.clone().unwrap_or_default();
    merged_env.extend(env.iter().map(|(name, value)| (name.clone(), value.clone())));
    Ok(AppliedAuth {
        model: request_model,
        api_key: api_key.or(resolved.auth.api_key),
        headers: merged_headers,
        env: merged_env,
    })
}

/// A cancellation token that follows `signal`; the returned guard removes the listener when dropped.
fn follow_signal(signal: Option<&AbortSignal>) -> (CancellationToken, ListenerGuard) {
    let token = CancellationToken::new();
    let guard = match signal {
        Some(signal) => {
            let cancel = token.clone();
            let id = signal.add_listener(move |_| cancel.cancel());
            ListenerGuard(Some((signal.clone(), id)))
        }
        None => ListenerGuard(None),
    };
    (token, guard)
}

struct ListenerGuard(Option<(AbortSignal, u64)>);

impl Drop for ListenerGuard {
    fn drop(&mut self) {
        if let Some((signal, id)) = self.0.take() {
            signal.remove_listener(id);
        }
    }
}

/// Upstream `lazyStream`: return a stream now, run async setup behind it, and forward the inner stream.
fn lazy_stream(
    model: &Model,
    setup: impl Future<Output = Result<AssistantMessageEventStream, String>> + Send + 'static,
    guard: ListenerGuard,
) -> AssistantMessageEventStream {
    let (sender, stream) = event_channel();
    let model = model.clone();
    tokio::spawn(async move {
        let _guard = guard;
        let mut inner = setup.await.unwrap_or_else(|error| setup_error_stream(&model, error));
        while let Some(event) = inner.next().await {
            sender.push(event);
        }
    });
    stream
}

fn stream_options(options: SimpleStreamOptions, auth: &AppliedAuth, cancel: CancellationToken) -> StreamOptions {
    StreamOptions {
        api_key: auth.api_key.clone(),
        cancel,
        telemetry: options.telemetry,
        max_tokens: options.max_tokens,
        temperature: options.temperature,
        reasoning: options.reasoning,
        thinking_budgets: options.thinking_budgets,
        headers: auth.headers.clone(),
        session_id: options.session_id,
        env: auth.env.clone(),
        transport: options.transport,
        cache_retention: options.cache_retention,
        sampling_params: options.sampling_params,
        timeout_ms: options.timeout_ms,
        max_retries: options.max_retries,
        max_retry_delay_ms: options.max_retry_delay_ms,
        websocket_connect_timeout_ms: options.websocket_connect_timeout_ms,
        tool_choice: options.tool_choice,
        metadata: options.metadata,
        deferred: options.deferred.as_ref().and_then(|deferred| deferred.to_stream_option()),
        on_payload: options.on_payload,
        on_response: options.on_response,
        ..StreamOptions::default()
    }
}

fn deferred_options(
    options: DeferredFetchOptions,
    auth: &AppliedAuth,
    cancel: CancellationToken,
) -> pi_ai::DeferredOptions {
    pi_ai::DeferredOptions {
        request: RequestOptions {
            api_key: auth.api_key.clone(),
            cancel,
            telemetry: options.telemetry,
            env: auth.env.clone(),
            headers: auth.headers.clone(),
            timeout_ms: options.timeout_ms,
            max_retries: options.max_retries,
            max_retry_delay_ms: options.max_retry_delay_ms,
            on_response: options.on_response,
            ..RequestOptions::default()
        },
        wait_ms: options.wait_ms,
    }
}

impl RegistryModels {
    /// The provider set for `model.provider`, or an error when neither it nor the catalog knows the provider.
    fn route(&self, model: &Model) -> Result<Option<Arc<dyn Models>>, String> {
        if let Some(provider) = self.provider(&model.provider) {
            return Ok(Some(provider));
        }
        if !self.knows_provider(&model.provider) {
            return Err(format!("Unknown provider: {}", model.provider));
        }
        Ok(None)
    }
}

impl Models for RegistryModels {
    fn get_models(&self, provider: Option<&str>) -> Vec<Model> {
        let providers = self.providers.read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
        let registry = self.registry.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut models: Vec<Model> = registry
            .models()
            .into_iter()
            .filter(|model| !providers.contains_key(&model.provider))
            .filter(|model| provider.is_none_or(|provider| model.provider == provider))
            .cloned()
            .collect();
        for (id, set) in &providers {
            if provider.is_none_or(|provider| provider == id) {
                models.extend(set.get_models(Some(id)));
            }
        }
        models
    }

    fn get_model(&self, provider: &str, id: &str) -> Option<Model> {
        if let Some(set) = self.provider(provider) {
            return set.get_model(provider, id);
        }
        self.registry.read().unwrap_or_else(|poisoned| poisoned.into_inner()).find(provider, id).cloned()
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        match self.route(model) {
            Err(error) => return setup_error_stream(model, error),
            Ok(Some(provider)) => return provider.stream_simple(model, context, options),
            Ok(None) => {}
        }
        let resolver = self.resolver.clone();
        let (cancel, guard) = follow_signal(options.signal.as_ref());
        let (model, context) = (model.clone(), context.clone());
        let setup_model = model.clone();
        let setup = async move {
            let auth = apply_auth(&resolver, &model, options.api_key.clone(), &options.env, &options.headers).await?;
            let request = stream_options(options, &auth, cancel);
            Ok(pi_ai::stream(&auth.model, &context, request))
        };
        lazy_stream(&setup_model, setup, guard)
    }

    fn stream_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: DeferredFetchOptions,
    ) -> AssistantMessageEventStream {
        match self.route(model) {
            Err(error) => return setup_error_stream(model, error),
            Ok(Some(provider)) => return provider.stream_deferred(model, handle, options),
            Ok(None) => {}
        }
        let resolver = self.resolver.clone();
        let (cancel, guard) = follow_signal(options.signal.as_ref());
        let (model, handle) = (model.clone(), handle.clone());
        let setup_model = model.clone();
        let setup = async move {
            let auth = apply_auth(&resolver, &model, options.api_key.clone(), &options.env, &options.headers).await?;
            let request = deferred_options(options, &auth, cancel);
            Ok(pi_ai::stream_deferred(&auth.model, &handle, request))
        };
        lazy_stream(&setup_model, setup, guard)
    }

    fn cancel_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: DeferredCancelOptions,
    ) -> BoxFuture<'static, DurableResult<()>> {
        match self.route(model) {
            Err(error) => return Box::pin(async move { Err(DurableError::error(error)) }),
            Ok(Some(provider)) => return provider.cancel_deferred(model, handle, options),
            Ok(None) => {}
        }
        let resolver = self.resolver.clone();
        let (model, handle) = (model.clone(), handle.clone());
        Box::pin(async move {
            let (cancel, _guard) = follow_signal(options.signal.as_ref());
            let auth = apply_auth(&resolver, &model, options.api_key.clone(), &options.env, &options.headers)
                .await
                .map_err(DurableError::error)?;
            let request = deferred_options(options, &auth, cancel);
            pi_ai::cancel_deferred(&auth.model, &handle, request).await.map_err(DurableError::error)
        })
    }
}
