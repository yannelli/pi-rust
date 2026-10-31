//! Providers, API implementations, OAuth, and virtual models from extensions (upstream
//! `pi.registerProvider`, `registerApiProvider`, and `pi.registerVirtualModel`). JavaScript
//! streams feed Rust event channels through `stream.event` sinks.

use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use pi_ai::auth::{
    AuthError, AuthEvent, AuthInteraction, AuthPrompt, AuthResult, LoginOptions, ModelAuth, OAuthAuth, OAuthCredential,
};
use pi_ai::{ApiProvider, AssistantMessageEventStream, Context, Model, StopReason, StreamOptions, error_stream};
use pi_js::{HostValue, JsError};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::convert::stream_options_to_json;
use super::ops::{Args, error};
use super::runner::ExtensionRunner;
use crate::agent_session::lock;
use crate::virtual_models::{ModelLookup, ModelRoute, ModelRouteRequest, ModelRouter, VirtualModelDefinition};

/// Which JavaScript function streams: a provider's `streamSimple` or a `registerApiProvider` API.
#[derive(Clone)]
enum StreamTarget {
    Provider(String),
    Api(String),
}

struct JsApiProvider {
    runner: Weak<ExtensionRunner>,
    target: StreamTarget,
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
        let (method, name) = match &self.target {
            StreamTarget::Provider(name) => ("providerStream", name.clone()),
            StreamTarget::Api(api) => ("apiStream", api.clone()),
        };
        let args =
            vec![json!(name), json!(model), json!(context), stream_options_to_json(&options, signal), json!(key)];
        let model = model.clone();
        handle.spawn(async move {
            let result = runner.call_json(method, args).await;
            lock(&runner.sinks).remove(&key);
            runner.release_signal(signal);
            if let Err(failure) = result {
                let mut message = pi_ai::AssistantMessage::empty(&model);
                let aborted = options.cancel.is_cancelled();
                message.stop_reason = if aborted { StopReason::Aborted } else { StopReason::Error };
                message.error_message = Some(failure.message);
                sender.push(pi_ai::AssistantMessageEvent::Error { reason: message.stop_reason, error: message });
            }
        });
        stream
    }
}

/// A JavaScript `oauth` config of a registered provider.
struct JsOAuth {
    runner: Weak<ExtensionRunner>,
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

/// A login callback from JavaScript, answered by the login's interaction.
pub(crate) struct LoginRequest {
    name: String,
    args: Vec<Value>,
    reply: oneshot::Sender<Result<Value, String>>,
}

impl JsOAuth {
    fn runner(&self) -> AuthResult<Arc<ExtensionRunner>> {
        self.runner.upgrade().ok_or_else(|| AuthError::Other("The extension host has shut down".into()))
    }

    async fn call(&self, method: &str, args: Vec<Value>) -> AuthResult<Value> {
        let runner = self.runner()?;
        let call = vec![json!(self.provider), json!(method), Value::Array(args)];
        runner.call_json("oauth", call).await.map_err(|failure| AuthError::Other(failure.message))
    }
}

fn credential(value: Value) -> AuthResult<OAuthCredential> {
    serde_json::from_value(value).map_err(|failure| AuthError::Other(format!("Invalid OAuth credential: {failure}")))
}

async fn answer(
    interaction: &dyn AuthInteraction,
    request: &LoginRequest,
    cancel: &CancellationToken,
) -> Result<Value, String> {
    let arg = request.args.first().cloned().unwrap_or(Value::Null);
    let text = |key: &str| arg[key].as_str().map(str::to_string);
    let prompt = match request.name.as_str() {
        "onAuth" => {
            let url = text("url").unwrap_or_default();
            interaction.notify(AuthEvent::AuthUrl { url, instructions: text("instructions") });
            return Ok(Value::Null);
        }
        "onDeviceCode" => {
            interaction.notify(AuthEvent::DeviceCode {
                user_code: text("userCode").unwrap_or_default(),
                verification_uri: text("verificationUri").unwrap_or_default(),
                open_browser_on_confirm: arg["openBrowserOnConfirm"].as_bool(),
                copy_code_on_confirm: arg["copyCodeOnConfirm"].as_bool(),
                interval_seconds: arg["intervalSeconds"].as_u64(),
                expires_in_seconds: arg["expiresInSeconds"].as_u64(),
            });
            return Ok(Value::Null);
        }
        "onProgress" => {
            let message = arg.as_str().map(str::to_string).unwrap_or_default();
            interaction.notify(AuthEvent::Progress { message });
            return Ok(Value::Null);
        }
        "onPrompt" => {
            AuthPrompt::Text { message: text("message").unwrap_or_default(), placeholder: text("placeholder") }
        }
        "onSelect" => AuthPrompt::Select {
            message: text("message").unwrap_or_default(),
            options: serde_json::from_value(arg["options"].clone()).unwrap_or_default(),
        },
        _ => AuthPrompt::ManualCode { message: "Paste the authorization code".into(), placeholder: None },
    };
    interaction.prompt(prompt, cancel.child_token()).await.map(Value::from).map_err(|failure| failure.to_string())
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
        let (sender, mut requests) = mpsc::unbounded_channel::<LoginRequest>();
        let key = runner.next_id();
        lock(&runner.logins).insert(key, sender);
        let signal = runner.signal_for(&cancel);
        let login = self.call("login", vec![json!({"key": key, "signal": signal})]);
        tokio::pin!(login);
        let result = loop {
            tokio::select! {
                result = &mut login => break result,
                Some(request) = requests.recv() => {
                    let reply = answer(interaction, &request, &cancel).await;
                    let _ = request.reply.send(reply);
                }
            }
        };
        lock(&runner.logins).remove(&key);
        runner.release_signal(signal);
        if cancel.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        credential(result?)
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
        let call = vec![json!(self.provider).into(), json!("getApiKey").into(), json!([credential]).into()];
        let result = runner.host().blocking_call_method_within("oauth", call, Duration::from_secs(10));
        let api_key = match result {
            Ok(HostValue::Json(Value::String(key))) => Some(key),
            _ => None,
        };
        ModelAuth { api_key, ..ModelAuth::default() }
    }
}

/// Queues a login callback in call order; the receiver gets its answer.
fn queue_login_callback(
    runner: &ExtensionRunner,
    args: &Args,
) -> Result<oneshot::Receiver<Result<Value, String>>, JsError> {
    let key = args.json(0)["key"].as_u64().unwrap_or(0);
    let name = args.str(1)?;
    let rest = args.0.iter().skip(2).cloned().map(HostValue::into_json).collect();
    let sender = lock(&runner.logins).get(&key).cloned().ok_or_else(|| error("The login has finished"))?;
    let (reply, answer) = oneshot::channel();
    sender.send(LoginRequest { name, args: rest, reply }).map_err(|_| error("The login has finished"))?;
    Ok(answer)
}

/// `oauth.notify`: a login callback with no answer, such as `onAuth` or `onProgress`.
pub(crate) fn oauth_notify(runner: &ExtensionRunner, args: &Args) -> Result<Value, JsError> {
    queue_login_callback(runner, args).map(|_| Value::Null)
}

/// `oauth.callback`: a login callback forwarded to the waiting login.
pub(crate) async fn oauth_callback(runner: &Arc<ExtensionRunner>, args: &Args) -> Result<Value, JsError> {
    let answer = queue_login_callback(runner, args)?;
    answer.await.map_err(|_| error("The login has finished"))?.map_err(error)
}

/// A JavaScript `route(request, ctx)` of a virtual model.
struct JsRouter {
    runner: Weak<ExtensionRunner>,
    key: String,
}

#[async_trait]
impl ModelRouter for JsRouter {
    async fn route(&self, request: ModelRouteRequest, models: &dyn ModelLookup) -> anyhow::Result<ModelRoute> {
        let runner = self.runner.upgrade().ok_or_else(|| anyhow::anyhow!("The extension host has shut down"))?;
        let signal = runner.signal_for(&request.cancel);
        let mut value = request.to_json();
        value["signal"] = json!(signal);
        let result = runner.call_json("route", vec![json!(self.key), value]).await;
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
        let runtime = self.model_runtime().clone();
        let builtin_id = config["oauth"]["builtin"].as_str();
        let builtin = builtin_id.map(|id| {
            runtime.builtin_oauth(id).ok_or_else(|| format!("Provider {name}: no built-in OAuth login \"{id}\""))
        });
        let builtin = builtin.transpose()?;
        if config["hasStreamSimple"].as_bool() == Some(true) {
            let api = config["api"]
                .as_str()
                .ok_or(format!("Provider {name}: \"api\" is required when registering streamSimple."))?;
            let provider = JsApiProvider { runner: self.weak(), target: StreamTarget::Provider(name.to_string()) };
            self.install_api(api, Arc::new(provider), Some(name));
        }
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
        let hooks = super::provider_hooks::JsProviderHooks::from_config(self.weak(), name, &config);
        runtime.register_extension_hooks(name, hooks.map(|hooks| Arc::new(hooks) as _));
        if let Some(oauth) = config["oauth"].as_object() {
            let display = oauth.get("name").and_then(Value::as_str).unwrap_or(name).to_string();
            let flow: Arc<dyn OAuthAuth> = match builtin {
                Some(inner) => Arc::new(BuiltinOAuth { inner, name: display }),
                None => Arc::new(JsOAuth { runner: self.weak(), provider: name.to_string(), name: display }),
            };
            runtime.register_extension_oauth(name, Some(flow));
        }
        // A built-in login also reads usage with that provider's fetcher (when it has one).
        runtime.usage().set_alias(name, builtin_id);
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
        let apis: Vec<String> = lock(&self.apis)
            .iter()
            .filter(|(_, (_, owner))| owner.as_deref() == Some(name))
            .map(|(api, _)| api.clone())
            .collect();
        for api in apis {
            self.restore_api(&api);
        }
        self.model_runtime().unregister_extension_provider(name);
        self.model_runtime().register_extension_hooks(name, None);
        self.model_runtime().register_extension_oauth(name, None);
        self.model_runtime().usage().set_alias(name, None);
        self.model_runtime().refresh_offline_in_background();
        if let Some(session) = self.session() {
            session.refresh_model_from_registry();
        }
    }

    /// Providers this runner registered, in registration order.
    pub(crate) fn provider_ids(&self) -> Vec<String> {
        lock(&self.providers).iter().map(|(name, _)| name.clone()).collect()
    }

    fn install_api(&self, api: &str, provider: Arc<dyn ApiProvider>, owner: Option<&str>) {
        let mut apis = lock(&self.apis);
        let previous = match apis.remove(api) {
            Some((previous, _)) => previous,
            None => pi_ai::api_provider(api),
        };
        apis.insert(api.to_string(), (previous, owner.map(str::to_string)));
        pi_ai::register_api_provider(api, provider);
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
            let provider = JsApiProvider { runner: self.weak(), target: StreamTarget::Api(api.to_string()) };
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
        let text = |key: &str| definition[key].as_str().unwrap_or_default().to_string();
        let (provider, id) = (text("provider"), text("id"));
        let router = JsRouter { runner: self.weak(), key: format!("{provider}/{id}") };
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
