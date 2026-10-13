//! The JavaScript parts of extension providers the model runtime calls (upstream
//! `composeModelProvider`): `refreshModels`, `oauth.modifyModels`, and the provider's `images`
//! and `classifiers` implementations.

use std::sync::Arc;

use async_trait::async_trait;
use pi_ai::Model;
use pi_ai::auth::{Credential, OAuthCredential};
use pi_ai::classify::{ClassifierContext, ClassifierOptions, ClassifierResult};
use pi_ai::images::{AssistantImages, ImagesContext, ImagesOptions};
use pi_ai::model_operations::{classifier_error_result, image_error_result};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::model_runtime::{ModelRuntime, read, write};
use super::runtime_extensions::model_kind;
use crate::catalog::{RefreshOptions, RefreshResult};

/// What `refreshModels(context)` receives (upstream `RefreshModelsContext`).
#[derive(Clone, Debug, Default)]
pub struct RefreshModelsContext {
    /// The provider's stored credential.
    pub credential: Option<Credential>,
    pub allow_network: bool,
    pub force: bool,
}

/// Functions of a `pi.registerProvider` config, implemented by the extension host.
#[async_trait]
pub trait ExtensionProviderHooks: Send + Sync {
    fn has_refresh_models(&self) -> bool;
    fn has_modify_models(&self) -> bool;
    /// APIs of the provider's `images` implementations.
    fn image_apis(&self) -> Vec<String>;
    /// APIs of the provider's `classifiers` implementations.
    fn classifier_apis(&self) -> Vec<String>;
    /// `refreshModels(context)`: the new model definitions.
    async fn refresh_models(
        &self,
        context: RefreshModelsContext,
        cancel: CancellationToken,
    ) -> Result<Vec<Value>, String>;
    /// `oauth.modifyModels(models, credential)` over the chat models.
    async fn modify_models(&self, models: Vec<Model>, credential: &OAuthCredential) -> Result<Vec<Model>, String>;
    async fn generate_images(
        &self,
        api: &str,
        model: &Model,
        context: &ImagesContext,
        options: ImagesOptions,
    ) -> AssistantImages;
    async fn classify(
        &self,
        api: &str,
        model: &Model,
        context: &ClassifierContext,
        options: ClassifierOptions,
    ) -> ClassifierResult;
}

/// An extension provider's hooks with the results of its last refresh.
pub(crate) struct ExtensionHookState {
    pub hooks: Arc<dyn ExtensionProviderHooks>,
    /// Model definitions `refreshModels` returned, in place of the registered `models`.
    pub refreshed: Option<Vec<Value>>,
    /// Chat models after `oauth.modifyModels` with the provider's OAuth credential.
    pub modified: Option<Vec<Model>>,
}

impl ModelRuntime {
    /// Set or remove the hooks of an extension provider.
    pub fn register_extension_hooks(&self, provider: &str, hooks: Option<Arc<dyn ExtensionProviderHooks>>) {
        let mut registered = write(&self.extension_hooks);
        match hooks {
            Some(hooks) => {
                registered.insert(provider.into(), ExtensionHookState { hooks, refreshed: None, modified: None });
            }
            None => drop(registered.remove(provider)),
        }
    }

    fn hooks_of(&self, provider: &str) -> Option<Arc<dyn ExtensionProviderHooks>> {
        read(&self.extension_hooks).get(provider).map(|state| state.hooks.clone())
    }

    pub(super) fn has_extension_refresh(&self) -> bool {
        read(&self.extension_hooks)
            .values()
            .any(|state| state.hooks.has_refresh_models() || state.hooks.has_modify_models())
    }

    /// Generate images through the provider of `model`: the extension's implementation for its
    /// API, then the built-in one.
    pub async fn generate_images(
        &self,
        model: &Model,
        context: &ImagesContext,
        options: ImagesOptions,
    ) -> AssistantImages {
        let hooks = self.hooks_of(&model.provider).filter(|hooks| !hooks.image_apis().is_empty());
        let Some(hooks) = hooks else { return pi_ai::images::generate_images(model, context, options).await };
        if hooks.image_apis().contains(&model.api) {
            return hooks.generate_images(&model.api, model, context, options).await;
        }
        if pi_ai::images::images_api(&model.api).is_some() {
            return pi_ai::images::generate_images(model, context, options).await;
        }
        let message = format!("Provider {} has no image implementation for \"{}\"", model.provider, model.api);
        image_error_result(model, message, options.request.cancel.is_cancelled())
    }

    /// Classify through the provider of `model`: the extension's implementation for its API,
    /// then the built-in one.
    pub async fn classify(
        &self,
        model: &Model,
        context: &ClassifierContext,
        options: ClassifierOptions,
    ) -> ClassifierResult {
        let hooks = self.hooks_of(&model.provider).filter(|hooks| !hooks.classifier_apis().is_empty());
        let Some(hooks) = hooks else { return pi_ai::classify::classify(model, context, options).await };
        if hooks.classifier_apis().contains(&model.api) {
            return hooks.classify(&model.api, model, context, options).await;
        }
        if pi_ai::classify::classifier_api(&model.api).is_some() {
            return pi_ai::classify::classify(model, context, options).await;
        }
        let message = format!("Provider {} has no classifier implementation for \"{}\"", model.provider, model.api);
        classifier_error_result(model, message, options.request.cancel.is_cancelled())
    }

    /// The `refreshModels` and `oauth.modifyModels` phase of a refresh, for extension providers.
    pub(super) async fn refresh_extension_providers(&self, options: &RefreshOptions, result: &mut RefreshResult) {
        let providers: Vec<(String, Arc<dyn ExtensionProviderHooks>)> = read(&self.extension_hooks)
            .iter()
            .filter(|(id, _)| options.providers.as_ref().is_none_or(|wanted| wanted.contains(id)))
            .map(|(id, state)| (id.clone(), state.hooks.clone()))
            .collect();
        for (provider, hooks) in providers {
            if !hooks.has_refresh_models() && !hooks.has_modify_models() {
                continue;
            }
            let credential = self.resolver().store().read(&provider).await.ok().flatten();
            let mut refreshed = None;
            if hooks.has_refresh_models() {
                let context = RefreshModelsContext {
                    credential: credential.clone(),
                    allow_network: options.allow_network,
                    force: options.force,
                };
                match hooks.refresh_models(context, options.cancel.clone()).await {
                    Ok(models) => refreshed = Some(models),
                    Err(error) => {
                        result.errors.insert(provider.clone(), error);
                        continue;
                    }
                }
            }
            if options.cancel.is_cancelled() {
                result.aborted = true;
                return;
            }
            let modified = match (&credential, hooks.has_modify_models()) {
                (Some(Credential::OAuth(oauth)), true) => {
                    let chat = self.provider_chat_models(&provider, refreshed.as_deref());
                    match chat {
                        Ok(chat) => hooks.modify_models(chat, oauth).await.map(Some),
                        Err(error) => Err(error),
                    }
                }
                _ => Ok(None),
            };
            match modified {
                Ok(modified) => {
                    if let Some(state) = write(&self.extension_hooks).get_mut(&provider) {
                        if refreshed.is_some() {
                            state.refreshed = refreshed;
                        }
                        state.modified = modified;
                    }
                }
                Err(error) => drop(result.errors.insert(provider, error)),
            }
        }
    }

    /// The chat models `provider` has with `refreshed` definitions, before `modifyModels`;
    /// an invalid refreshed list is an error.
    fn provider_chat_models(&self, provider: &str, refreshed: Option<&[Value]>) -> Result<Vec<Model>, String> {
        let config = read(&self.extension_providers).get(provider).cloned().unwrap_or_default();
        let mut registry = self.base_registry();
        super::runtime_extensions::apply_extension_provider(&mut registry, provider, &config, refreshed)?;
        let models = registry.provider_models(provider);
        Ok(models.into_iter().filter(|model| model_kind(model) == "chat").collect())
    }
}
