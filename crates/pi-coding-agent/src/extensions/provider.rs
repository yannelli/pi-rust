//! Providers, API implementations, OAuth, and virtual models from extensions (upstream
//! `pi.registerProvider`, `registerApiProvider`, and `pi.registerVirtualModel`). JavaScript
//! streams feed Rust event channels through `stream.event` sinks.

use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use pi_ai::auth::{AuthError, AuthInteraction, AuthResult, LoginOptions, ModelAuth, OAuthAuth, OAuthCredential};
use pi_ai::{
    ApiProvider, AssistantMessageEventStream, Context, MessageBuilder, Model, StopReason, StreamOptions, error_stream,
};
use pi_js::HostValue;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::convert::stream_options_to_json;
use super::login_bridge::drive_login;
use super::runner::ExtensionRunner;
use crate::agent_session::lock;
use crate::virtual_models::{ModelLookup, ModelRoute, ModelRouteRequest, ModelRouter, VirtualModelDefinition};

/// Which JavaScript function streams: a provider's `streamSimple`, a `registerApiProvider` API, or
/// a native `Provider` (its object under `target`, see `native_provider.rs`).
#[derive(Clone)]
pub(super) enum StreamTarget {
    Provider(String),
    Api(String),
    Native { target: u64, id: String },
}

pub(super) struct JsApiProvider {
    pub runner: Weak<ExtensionRunner>,
    pub group: u64,
    pub target: StreamTarget,
}

impl ApiProvider for JsApiProvider {
    fn stream(&self, model: &Model, context: &Context, options: StreamOptions) -> AssistantMessageEventStream {
        let (Some(runner), Ok(handle)) = (self.runner.upgrade(), tokio::runtime::Handle::try_current()) else {
            return error_stream(model, StopReason::Error, "The extension that registered this API has shut down");
        };
        let (sender, stream) = pi_ai::event_channel();
        let key = runner.next_id();
        lock(&runner.sinks).insert(key, sender.clone());
        let signal = runner.signal_for(&options.cancel);
        let (method, mut args) = match &self.target {
            StreamTarget::Provider(name) => ("providerStream", vec![json!(name)]),
            StreamTarget::Api(api) => ("apiStream", vec![json!(api)]),
            StreamTarget::Native { target, id } => ("nativeStream", vec![json!(target), json!(id)]),
        };
        args.extend([json!(model), json!(context), stream_options_to_json(&options, signal), json!(key)]);
        let (model, group) = (model.clone(), self.group);
        handle.spawn(async move {
            let result = runner.call_model(group, method, args).await;
            lock(&runner.sinks).remove(&key);
            runner.release_signal(signal);
            // A native stream reports `false` when it ended without `done` or `error`.
            let failure = match result {
                Err(failure) => Some(failure.message),
                Ok(Value::Bool(false)) => Some("The provider stream ended without a done or error event".into()),
                Ok(_) => None,
            };
            if let Some(message) = failure {
                let reason = if options.cancel.is_cancelled() { StopReason::Aborted } else { StopReason::Error };
                MessageBuilder::new(&model, sender).fail(reason, message);
            }
        });
        stream
    }
}

/// A JavaScript `oauth` config of a registered provider.
struct JsOAuth {
    runner: Weak<ExtensionRunner>,
    group: u64,
    provider: String,
    name: String,
}

/// A core OAuth flow under the name of the extension provider that reuses it: login and refresh
/// run in pi, and the credential is stored under the extension provider's id.
struct BuiltinOAuth {
    inner: Arc<dyn OAuthAuth>,
    name: String,
}

#[async_trait]
impl OAuthAuth for BuiltinOAuth {
    fn name(&self) -> &str {
        &self.name
    }

    fn is_subscription(&self) -> bool {
        self.inner.is_subscription()
    }

    fn login_label(&self) -> Option<&str> {
        Some(&self.name)
    }

    async fn login(
        &self,
        interaction: &dyn AuthInteraction,
        options: &LoginOptions,
        cancel: CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        self.inner.login(interaction, options, cancel).await
    }

    async fn refresh(&self, credential: &OAuthCredential, cancel: CancellationToken) -> AuthResult<OAuthCredential> {
        self.inner.refresh(credential, cancel).await
    }

    fn to_auth(&self, credential: &OAuthCredential) -> ModelAuth {
        self.inner.to_auth(credential)
    }
}

impl JsOAuth {
    fn runner(&self) -> AuthResult<Arc<ExtensionRunner>> {
        self.runner.upgrade().ok_or_else(|| AuthError::Other("The extension host has shut down".into()))
    }

    async fn call(&self, method: &str, args: Vec<Value>) -> AuthResult<Value> {
        let runner = self.runner()?;
        let call = vec![json!(self.provider), json!(method), Value::Array(args)];
        runner.call_model(self.group, "oauth", call).await.map_err(|failure| AuthError::Other(failure.message))
    }
}

fn credential(value: Value) -> AuthResult<OAuthCredential> {
    serde_json::from_value(value).map_err(|failure| AuthError::Other(format!("Invalid OAuth credential: {failure}")))
}

#[async_trait]
impl OAuthAuth for JsOAuth {
    fn name(&self) -> &str {
        &self.name
    }

    async fn login(
        &self,
        interaction: &dyn AuthInteraction,
        _options: &LoginOptions,
        cancel: CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        let runner = self.runner()?;
        let login = drive_login(&runner, interaction, &cancel, |key, signal| {
            self.call("login", vec![json!({"key": key, "signal": signal})])
        });
        credential(login.await?)
    }

    async fn refresh(
        &self,
        credential_value: &OAuthCredential,
        cancel: CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        let runner = self.runner()?;
        let signal = runner.signal_for(&cancel);
        let result = self.call("refreshToken", vec![json!(credential_value), json!(signal)]).await;
        runner.release_signal(signal);
        credential(result?)
    }

    fn to_auth(&self, credential: &OAuthCredential) -> ModelAuth {
        let Ok(runner) = self.runner() else { return ModelAuth::default() };
        let call: Vec<HostValue> =
            vec![json!(self.provider).into(), json!("getApiKey").into(), json!([credential]).into()];
        let (method, call) = match self.group {
            0 => ("oauth", call),
            group => ("groupCallback", [json!(group).into(), json!("oauth").into()].into_iter().chain(call).collect()),
        };
        let result = runner.host().blocking_call_method_within(method, call, Duration::from_secs(10));
        let api_key = match result {
            Ok(HostValue::Json(Value::String(key))) => Some(key),
            _ => None,
        };
        ModelAuth { api_key, ..ModelAuth::default() }
    }
}

/// A JavaScript `route(request, ctx)` of a virtual model.
struct JsRouter {
    runner: Weak<ExtensionRunner>,
    group: u64,
    key: String,
}

#[async_trait]
impl ModelRouter for JsRouter {
    async fn route(&self, request: ModelRouteRequest, models: &dyn ModelLookup) -> anyhow::Result<ModelRoute> {
        let runner = self.runner.upgrade().ok_or_else(|| anyhow::anyhow!("The extension host has shut down"))?;
        let signal = runner.signal_for(&request.cancel);
        let mut value = request.to_json();
        value["signal"] = json!(signal);
        let result = runner.call_model(self.group, "route", vec![json!(self.key), value]).await;
        runner.release_signal(signal);
        let route = result.map_err(|failure| anyhow::anyhow!(failure.message))?;
        let target = &route["model"];
        let (provider, id) =
            (target["provider"].as_str().unwrap_or_default(), target["id"].as_str().unwrap_or_default());
        let model = match models.find(provider, id) {
            Some(model) => model,
            None => serde_json::from_value(target.clone())
                .map_err(|_| anyhow::anyhow!("Virtual model route returned an unknown model {provider}/{id}"))?,
        };
        let thinking_level = super::convert::thinking_level(&route["thinkingLevel"]).unwrap_or(request.thinking_level);
        let state = route.get("state").filter(|state| !state.is_null()).cloned();
        Ok(ModelRoute { model, thinking_level, state })
    }
}

impl ExtensionRunner {
    /// `pi.registerProvider`: models and auth settings, a `streamSimple` API, and OAuth.
    pub(crate) fn register_provider(&self, name: &str, config: Value, extension_path: &str) -> Result<(), String> {
        self.register_provider_for(&self.this(), 0, name, config, extension_path)
    }

    /// Register with this runner's runtime; JavaScript callbacks go to factory `group` of `callbacks`,
    /// which also owns the API it installs (an SDK child host binding its parent's factories).
    pub(crate) fn register_provider_for(
        &self,
        callbacks: &Arc<Self>,
        group: u64,
        name: &str,
        config: Value,
        extension_path: &str,
    ) -> Result<(), String> {
        let runtime = self.model_runtime().clone();
        let builtin_id = config["oauth"]["builtin"].as_str();
        let builtin = builtin_id.map(|id| {
            runtime.builtin_oauth(id).ok_or_else(|| format!("Provider {name}: no built-in OAuth login \"{id}\""))
        });
        let builtin = builtin.transpose()?;
        let stream_api = if config["hasStreamSimple"].as_bool() == Some(true) {
            Some(
                config["api"]
                    .as_str()
                    .ok_or(format!("Provider {name}: \"api\" is required when registering streamSimple."))?,
            )
        } else {
            None
        };
        let mut stored = config.clone();
        if let Some(object) = stored.as_object_mut() {
            for key in [
                "hasStreamSimple",
                "hasHeaderFunction",
                "oauth",
                "streamSimple",
                "hasRefreshModels",
                "imageApis",
                "classifierApis",
            ] {
                object.remove(key);
            }
        }
        runtime.register_extension_provider(name, stored)?;
        runtime.unregister_native_provider(name);
        if let Some(api) = stream_api {
            let target = StreamTarget::Provider(name.to_string());
            let provider = JsApiProvider { runner: callbacks.weak(), group, target };
            callbacks.install_api(api, Arc::new(provider), Some((group, name)));
        }
        let hooks = super::provider_hooks::JsProviderHooks::from_config(callbacks.weak(), group, name, &config);
        runtime.register_extension_hooks(name, hooks.map(|hooks| Arc::new(hooks) as _));
        if let Some(oauth) = config["oauth"].as_object() {
            let display = oauth.get("name").and_then(Value::as_str).unwrap_or(name).to_string();
            let flow: Arc<dyn OAuthAuth> = match builtin {
                Some(inner) => Arc::new(BuiltinOAuth { inner, name: display }),
                None => {
                    Arc::new(JsOAuth { runner: callbacks.weak(), group, provider: name.to_string(), name: display })
                }
            };
            runtime.register_extension_oauth(name, Some(flow));
        }
        // A built-in login also reads usage with that provider's fetcher (when it has one), and makes
        // the provider an account that `before_provider_auth` can pick for that provider's models.
        runtime.usage().set_alias(name, builtin_id);
        runtime.set_account_base(name, builtin_id);
        // This runs on the extension thread, and `refreshModels` calls back into it: spawn.
        runtime.refresh_offline_in_background();
        let mut providers = lock(&self.providers);
        providers.retain(|(existing, _)| existing != name);
        providers.push((name.to_string(), extension_path.to_string()));
        drop(providers);
        if let Some(session) = self.session() {
            session.refresh_model_from_registry();
        }
        Ok(())
    }

    pub(crate) fn unregister_provider(&self, name: &str) {
        lock(&self.providers).retain(|(existing, _)| existing != name);
        self.model_runtime().unregister_native_provider(name);
        self.restore_owned_apis(0, name);
        let parent = self.parent.get().and_then(|link| Some((link.parent.upgrade()?, link.group)));
        if let Some((parent, group)) = parent {
            parent.restore_owned_apis(group, name);
        }
        self.model_runtime().unregister_extension_provider(name);
        self.model_runtime().register_extension_hooks(name, None);
        self.model_runtime().register_extension_oauth(name, None);
        self.model_runtime().usage().set_alias(name, None);
        self.model_runtime().set_account_base(name, None);
        self.model_runtime().refresh_offline_in_background();
        if let Some(session) = self.session() {
            session.refresh_model_from_registry();
        }
    }

    /// Providers this runner registered, in registration order.
    pub(crate) fn provider_ids(&self) -> Vec<String> {
        lock(&self.providers).iter().map(|(name, _)| name.clone()).collect()
    }

    fn install_api(&self, api: &str, provider: Arc<dyn ApiProvider>, owner: Option<(u64, &str)>) {
        let mut apis = lock(&self.apis);
        let previous = match apis.remove(api) {
            Some((previous, _)) => previous,
            None => pi_ai::api_provider(api),
        };
        apis.insert(api.to_string(), (previous, owner.map(|(group, name)| (group, name.to_string()))));
        pi_ai::register_api_provider(api, provider);
    }

    /// Restore the APIs that provider `name` of factory `group` installed here.
    fn restore_owned_apis(&self, group: u64, name: &str) {
        let apis: Vec<String> = lock(&self.apis)
            .iter()
            .filter(|(_, (_, owner))| owner.as_ref().is_some_and(|owner| owner.0 == group && owner.1 == name))
            .map(|(api, _)| api.clone())
            .collect();
        for api in apis {
            self.restore_api(&api);
        }
    }

    fn restore_api(&self, api: &str) {
        let Some((previous, _)) = lock(&self.apis).remove(api) else { return };
        match previous {
            Some(previous) => pi_ai::register_api_provider(api, previous),
            None => drop(pi_ai::unregister_api_provider(api)),
        }
    }

    /// Restore every API this runner replaced (on shutdown).
    pub fn restore_apis(&self) {
        let apis: Vec<String> = lock(&self.apis).keys().cloned().collect();
        for api in apis {
            self.restore_api(&api);
        }
    }

    pub(crate) fn has_js_api(&self, api: &str) -> bool {
        lock(&self.apis).contains_key(api)
    }

    /// `registerApiProvider` / `unregisterApiProviders` from `@earendil-works/pi-ai`.
    pub(crate) fn set_js_api(&self, api: &str, registered: bool) {
        if registered {
            let provider = JsApiProvider { runner: self.weak(), group: 0, target: StreamTarget::Api(api.to_string()) };
            self.install_api(api, Arc::new(provider), None);
        } else {
            self.restore_api(api);
        }
    }

    /// A Rust implementation of `api`, ignoring the ones this runner installed.
    pub(crate) fn stream_api(
        &self,
        model: &Model,
        context: &Context,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let previous = lock(&self.apis).get(&model.api).map(|(previous, _)| previous.clone());
        match previous {
            Some(Some(provider)) => provider.stream(model, context, options),
            Some(None) => {
                error_stream(model, StopReason::Error, format!("No provider registered for API: {}", model.api))
            }
            None => pi_ai::stream(model, context, options),
        }
    }

    /// `pi.registerVirtualModel`: the definition without `route`, which stays in JavaScript.
    pub(crate) fn register_virtual_model(&self, definition: Value) -> Result<(), String> {
        self.register_virtual_model_for(&self.this(), 0, definition)
    }

    pub(crate) fn register_virtual_model_for(
        &self,
        callbacks: &Arc<Self>,
        group: u64,
        definition: Value,
    ) -> Result<(), String> {
        let text = |key: &str| definition[key].as_str().unwrap_or_default().to_string();
        let (provider, id) = (text("provider"), text("id"));
        let router = JsRouter { runner: callbacks.weak(), group, key: format!("{provider}/{id}") };
        let mut model =
            VirtualModelDefinition::new(&provider, &id, definition["name"].as_str().unwrap_or(&id), Arc::new(router));
        model.thinking_levels = definition["thinkingLevels"]
            .as_array()
            .map(|levels| levels.iter().filter_map(super::convert::thinking_level).collect());
        model.context_window = definition["contextWindow"].as_u64();
        model.max_tokens = definition["maxTokens"].as_u64();
        model.input = serde_json::from_value(definition["input"].clone()).ok();
        self.model_runtime().register_virtual_model(model).map_err(|failure| format!("{failure:#}"))?;
        lock(&self.virtual_models).push((provider, id));
        if let Some(session) = self.session() {
            session.refresh_model_from_registry();
        }
        Ok(())
    }

    pub(crate) fn unregister_virtual_model(&self, provider: &str, id: &str) {
        lock(&self.virtual_models).retain(|(p, i)| !(p == provider && i == id));
        self.model_runtime().unregister_virtual_model(provider, id);
        if let Some(session) = self.session() {
            session.refresh_model_from_registry();
        }
    }

    /// Remove what this runner registered in the model runtime (on reload or shutdown).
    pub(crate) fn unregister_models(&self) {
        let virtual_models: Vec<(String, String)> = lock(&self.virtual_models).drain(..).collect();
        let providers: Vec<String> = self.provider_ids();
        for (provider, id) in virtual_models {
            self.model_runtime().unregister_virtual_model(&provider, &id);
        }
        for name in providers {
            self.unregister_provider(&name);
        }
        self.restore_apis();
    }
}
