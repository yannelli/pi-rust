//! Virtual models in the model runtime (upstream `ModelRuntime.registerVirtualModel`,
//! `unregisterVirtualModel`, `resolveModel`, and `getPhysicalModel`).

use std::collections::HashSet;
use std::sync::Arc;

use pi_ai::models::clamp_thinking_level;
use pi_ai::{AssistantMessage, Message, Model, ModelRegistry, ThinkingLevel};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::model_runtime::{ModelRuntime, read, write};
use crate::virtual_models::{
    FailedRequest, ModelLookup, ModelRoute, ModelRouteReason, ModelRouteRequest, ModelRouter, RoutedModel,
    VirtualModelDefinition, create_virtual_model, find_latest_response, is_virtual_model,
};

/// A registered virtual model: its catalog entry and router.
#[derive(Clone)]
pub(crate) struct RegisteredVirtualModel {
    pub model: Model,
    pub router: Arc<dyn ModelRouter>,
}

/// Inputs of [`ModelRuntime::resolve_model`] besides the model and messages.
#[derive(Clone, Debug)]
pub struct RouteOptions {
    pub reason: ModelRouteReason,
    pub thinking_level: ThinkingLevel,
    pub cancel: CancellationToken,
    /// For `retry`: the failed response, which the messages no longer contain.
    pub failed: Option<AssistantMessage>,
    /// Router state stored by the caller, which also stores the returned state.
    pub state: Option<Value>,
}

impl RouteOptions {
    pub fn new(reason: ModelRouteReason, thinking_level: ThinkingLevel) -> Self {
        Self { reason, thinking_level, cancel: CancellationToken::new(), failed: None, state: None }
    }
}

impl ModelRuntime {
    /// Register a virtual model under `definition.provider`, which may also list physical models
    /// or several virtual models. Registering the same provider and id again replaces it.
    pub fn register_virtual_model(self: &Arc<Self>, definition: VirtualModelDefinition) -> anyhow::Result<()> {
        let (provider, id) = (definition.provider.clone(), definition.id.clone());
        anyhow::ensure!(
            !provider.trim().is_empty() && !id.trim().is_empty(),
            "Virtual model provider and id must not be empty."
        );
        if self.get_model(&provider, &id).is_some_and(|existing| !is_virtual_model(&existing)) {
            anyhow::bail!("Virtual model {provider}/{id} conflicts with a physical model.");
        }
        let model = create_virtual_model(&definition);
        write(&self.virtual_models).insert((provider, id), RegisteredVirtualModel { model, router: definition.router });
        self.apply_virtual_models();
        self.refresh_offline_in_background();
        Ok(())
    }

    /// Remove a virtual model; false when it was not registered.
    pub fn unregister_virtual_model(self: &Arc<Self>, provider: &str, id: &str) -> bool {
        let removed = write(&self.virtual_models).remove(&(provider.to_string(), id.to_string())).is_some();
        if removed {
            self.rebuild_registry();
            self.refresh_offline_in_background();
        }
        removed
    }

    /// Catalog entries of the registered virtual models.
    pub fn virtual_models(&self) -> Vec<Model> {
        read(&self.virtual_models).values().map(|entry| entry.model.clone()).collect()
    }

    /// Providers that list only virtual models; they need no credentials.
    pub(super) fn virtual_only_providers(&self) -> HashSet<String> {
        self.virtual_only_in(&self.registry())
    }

    fn virtual_only_in(&self, registry: &ModelRegistry) -> HashSet<String> {
        let providers: HashSet<String> =
            read(&self.virtual_models).keys().map(|(provider, _)| provider.clone()).collect();
        if providers.is_empty() {
            return providers;
        }
        let configs = registry.auth_configs();
        let physical: HashSet<&str> = registry
            .all_models()
            .iter()
            .filter(|model| !is_virtual_model(model))
            .map(|model| model.provider.as_str())
            .collect();
        providers
            .into_iter()
            .filter(|provider| !physical.contains(provider.as_str()) && !configs.contains_key(provider))
            .collect()
    }

    /// Add the virtual models to the registry; they hide physical chat models with their id.
    pub(super) fn apply_virtual_models(&self) {
        self.add_virtual_models(&mut self.registry_mut());
    }

    /// Add the virtual models to `registry` and mark its configured providers.
    pub(super) fn add_virtual_models(&self, registry: &mut ModelRegistry) {
        for model in self.virtual_models() {
            registry.upsert(model);
        }
        let mut configured = read(&self.configured).clone();
        configured.extend(self.virtual_only_in(registry));
        registry.set_configured_providers(configured);
        self.filter_disabled_providers(registry);
    }

    /// A catalog chat model that is not virtual.
    pub fn physical_model(&self, provider: &str, id: &str) -> Option<Model> {
        self.get_model(provider, id).filter(|model| !is_virtual_model(model))
    }

    /// Ask a virtual model's router for the model and thinking level of one request. The route
    /// must name a physical model with credentials; its thinking level is clamped to it.
    pub async fn resolve_model(
        &self,
        model: &Model,
        messages: Vec<Message>,
        options: RouteOptions,
    ) -> anyhow::Result<ModelRoute> {
        let name = format!("Virtual model {}/{}", model.provider, model.id);
        let router = read(&self.virtual_models)
            .get(&(model.provider.clone(), model.id.clone()))
            .map(|entry| entry.router.clone())
            .ok_or_else(|| anyhow::anyhow!("{name} is not registered."))?;
        let previous = find_latest_response(&messages).and_then(|latest| {
            let model = self.physical_model(&latest.provider, &latest.model)?;
            Some(RoutedModel { model, thinking_level: latest.thinking_level })
        });
        // Failed routing leaves the virtual model on its message; no physical request to report.
        let failed = options.failed.and_then(|message| {
            let model = self.physical_model(&message.provider, &message.model)?;
            Some(FailedRequest { model, thinking_level: message.thinking_level, message })
        });
        let request = ModelRouteRequest {
            model: model.clone(),
            thinking_level: options.thinking_level,
            reason: options.reason,
            previous,
            failed,
            state: options.state,
            messages,
            cancel: options.cancel,
        };
        let route = router.route(request, self).await?;
        let routed = format!("{name} routed to {}/{}", route.model.provider, route.model.id);
        let Some(target) = self.physical_model(&route.model.provider, &route.model.id) else {
            anyhow::bail!("{routed}, which is not a physical model.");
        };
        if !self.has_configured_auth(&target.provider) {
            anyhow::bail!("{routed}, which has no credentials.");
        }
        let thinking_level = clamp_thinking_level(&target, route.thinking_level);
        Ok(ModelRoute { model: target, thinking_level, state: route.state })
    }
}

impl ModelLookup for ModelRuntime {
    fn find(&self, provider: &str, id: &str) -> Option<Model> {
        self.physical_model(provider, id)
    }

    fn has_configured_auth(&self, provider: &str) -> bool {
        ModelRuntime::has_configured_auth(self, provider)
    }
}
