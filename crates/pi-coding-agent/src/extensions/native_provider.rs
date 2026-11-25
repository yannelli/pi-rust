//! Native `Provider` objects of extensions (upstream `pi.registerProvider(provider)` and
//! `ModelRuntime.registerNativeProvider`). The model runtime keeps the provider's models and
//! credentials; it calls the object's `streamSimple`, `auth`, `refreshModels`, `filterModels`,
//! `generateImages`, and `classify` in the JavaScript host that holds the object.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use pi_ai::auth::{
    ApiKeyCredential, AuthCheck, AuthError, AuthInteraction, AuthResult, AuthType, Credential, LoginOptions, ModelAuth,
    OAuthAuth, OAuthCredential, ResolvedAuth,
};
use pi_ai::classify::{ClassifierContext, ClassifierOptions, ClassifierResult};
use pi_ai::images::{AssistantImages, ImagesContext, ImagesOptions};
use pi_ai::model_operations::{classifier_error_result, image_error_result};
use pi_ai::{ApiProvider, AssistantMessageEventStream, Context, Model, RequestOptions, StreamOptions};
use pi_js::{HostValue, JsError};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::login_bridge::drive_login;
use super::ops::{Args, error};
use super::provider::{JsApiProvider, StreamTarget};
use super::runner::ExtensionRunner;
use crate::agent_session::{
    ModelRuntime, NativeProvider, NativeProviderInfo, NativeRefreshContext, NativeRegistration, ProviderAuth, lock,
};

type DeviceId = Arc<dyn Fn() -> String + Send + Sync>;
/// The runtime, provider id, and generation of a refresh in progress.
type RefreshHandle = (Weak<ModelRuntime>, String, u64);

/// Refreshes in progress, by the key their `publish` sends, and `getDeviceId` of running logins.
#[derive(Default)]
pub(crate) struct NativeState {
    refreshes: Mutex<HashMap<u64, RefreshHandle>>,
    devices: Mutex<HashMap<u64, DeviceId>>,
}

/// Where a provider object lives: factory `group` of `runner` holds it under `target`, 0 for
/// `pi.registerProvider` and otherwise the id of the SDK `ModelRuntime` it was registered with.
#[derive(Clone)]
struct Link {
    runner: Weak<ExtensionRunner>,
    group: u64,
    target: u64,
    id: String,
}

impl Link {
    fn runner(&self) -> Result<Arc<ExtensionRunner>, String> {
        self.runner.upgrade().ok_or_else(|| "The extension that registered this provider has shut down".to_string())
    }

    /// Call `method(target, id, ...args, signal)`; `signal` aborts with `cancel`.
    async fn call(&self, method: &str, args: Vec<Value>, cancel: &CancellationToken) -> Result<Value, String> {
        let runner = self.runner()?;
        let signal = runner.signal_for(cancel);
        let mut call = vec![json!(self.target), json!(self.id)];
        call.extend(args);
        call.push(json!(signal));
        let result = runner.call_model(self.group, method, call).await;
        runner.release_signal(signal);
        result.map_err(|failure| failure.message)
    }
}

/// The JSON shape of a stored credential, `type` included.
fn credential_json(credential: Option<&Credential>) -> Value {
    credential.map_or(Value::Null, |credential| json!(credential))
}

fn oauth_credential(mut value: Value) -> AuthResult<OAuthCredential> {
    if let Some(object) = value.as_object_mut() {
        object.remove("type");
    }
    serde_json::from_value(value).map_err(|failure| AuthError::other(format!("Invalid OAuth credential: {failure}")))
}

fn model_auth(value: &Value) -> ModelAuth {
    let headers = value["headers"].as_object().into_iter().flatten();
    ModelAuth {
        api_key: value["apiKey"].as_str().map(str::to_string),
        headers: headers.filter_map(|(name, header)| Some((name.clone(), header.as_str()?.to_string()))).collect(),
        base_url: value["baseUrl"].as_str().map(str::to_string),
    }
}

struct JsNativeProvider {
    link: Link,
    stream: JsApiProvider,
    runtime: Weak<ModelRuntime>,
}

fn request_json(request: &RequestOptions) -> Value {
    json!({"apiKey": request.api_key, "headers": request.headers, "env": request.env})
}

#[async_trait]
impl NativeProvider for JsNativeProvider {
    fn stream(&self, model: &Model, context: &Context, options: StreamOptions) -> AssistantMessageEventStream {
        self.stream.stream(model, context, options)
    }

    async fn refresh_models(
        &self,
        context: NativeRefreshContext,
        cancel: CancellationToken,
    ) -> Result<Vec<Value>, String> {
        let runner = self.link.runner()?;
        let key = runner.next_id();
        let handle = (self.runtime.clone(), self.link.id.clone(), context.generation);
        lock(&runner.native.refreshes).insert(key, handle);
        let value = json!({"credential": credential_json(context.credential.as_ref()), "stored": context.stored,
            "allowNetwork": context.allow_network, "force": context.force, "baseUrl": context.base_url, "publish": key});
        let result = self.link.call("nativeRefresh", vec![value], &cancel).await;
        lock(&runner.native.refreshes).remove(&key);
        match result? {
            Value::Array(models) => Ok(models),
            _ => Ok(Vec::new()),
        }
    }

    async fn filter_models(&self, credential: Option<Credential>) -> Result<Vec<(String, String)>, String> {
        let args = vec![credential_json(credential.as_ref())];
        let kept = self.link.call("nativeFilter", args, &CancellationToken::new()).await?;
        serde_json::from_value(kept).map_err(|failure| format!("Invalid filterModels result: {failure}"))
    }

    async fn generate_images(&self, model: &Model, context: &ImagesContext, options: ImagesOptions) -> AssistantImages {
        let cancel = options.request.cancel.clone();
        let mut request = request_json(&options.request);
        request["metadata"] = json!(options.metadata);
        match self.link.call("nativeImages", vec![json!(model), json!(context), request], &cancel).await {
            Ok(value) => serde_json::from_value(value)
                .unwrap_or_else(|failure| image_error_result(model, format!("Invalid image result: {failure}"), false)),
            Err(message) => image_error_result(model, message, cancel.is_cancelled()),
        }
    }

    async fn classify(
        &self,
        model: &Model,
        context: &ClassifierContext,
        options: ClassifierOptions,
    ) -> ClassifierResult {
        let cancel = options.request.cancel.clone();
        let mut request = request_json(&options.request);
        request["temperature"] = json!(options.temperature);
        match self.link.call("nativeClassify", vec![json!(model), json!(context), request], &cancel).await {
            Ok(value) => serde_json::from_value(value).unwrap_or_else(|failure| {
                classifier_error_result(model, format!("Invalid classifier result: {failure}"), false)
            }),
            Err(message) => classifier_error_result(model, message, cancel.is_cancelled()),
        }
    }
}

/// `auth.apiKey` of a provider object, and the OAuth status of its stored credential.
struct JsNativeAuth {
    link: Link,
    api_key: bool,
    login: bool,
    oauth: bool,
}

fn api_key_credential(stored: Option<&Credential>) -> Option<&Credential> {
    stored.filter(|stored| matches!(stored, Credential::ApiKey(_)))
}

#[async_trait]
impl ProviderAuth for JsNativeAuth {
    async fn check(&self, stored: Option<&Credential>) -> bool {
        self.check_source(stored).await.is_some()
    }

    async fn check_source(&self, stored: Option<&Credential>) -> Option<AuthCheck> {
        if matches!(stored, Some(Credential::OAuth(_))) {
            return self.oauth.then(|| AuthCheck { auth_type: AuthType::OAuth, source: "OAuth".into() });
        }
        if !self.api_key {
            return None;
        }
        let args = vec![json!("check"), credential_json(api_key_credential(stored))];
        let found = self.link.call("nativeAuth", args, &CancellationToken::new()).await.ok()?;
        let source = found.is_object().then(|| found["source"].as_str().unwrap_or_default().to_string())?;
        let auth_type = if found["type"] == "oauth" { AuthType::OAuth } else { AuthType::ApiKey };
        Some(AuthCheck { auth_type, source })
    }

    async fn resolve(&self, stored: Option<&Credential>) -> Result<Option<ResolvedAuth>, String> {
        if !self.api_key || matches!(stored, Some(Credential::OAuth(_))) {
            return Ok(None);
        }
        let args = vec![json!("resolve"), credential_json(api_key_credential(stored))];
        let result = self.link.call("nativeAuth", args, &CancellationToken::new()).await?;
        if !result.is_object() {
            return Ok(None);
        }
        Ok(Some(ResolvedAuth {
            auth: model_auth(&result["auth"]),
            env: serde_json::from_value(result["env"].clone()).ok(),
            source: result["source"].as_str().map(str::to_string),
        }))
    }

    fn has_api_key(&self) -> bool {
        self.api_key
    }

    fn has_login(&self) -> bool {
        self.login
    }

    async fn login(
        &self,
        interaction: &dyn AuthInteraction,
        cancel: CancellationToken,
    ) -> AuthResult<ApiKeyCredential> {
        let value = native_login(&self.link, "api_key", interaction, &LoginOptions::default(), &cancel).await?;
        match serde_json::from_value(value) {
            Ok(Credential::ApiKey(credential)) => Ok(credential),
            _ => Err(AuthError::other("API key login returned no api_key credential")),
        }
    }
}

/// Run `auth.apiKey.login` or `auth.oauth.login` of a provider object with `interaction`.
async fn native_login(
    link: &Link,
    kind: &str,
    interaction: &dyn AuthInteraction,
    options: &LoginOptions,
    cancel: &CancellationToken,
) -> AuthResult<Value> {
    let runner = link.runner().map_err(AuthError::Other)?;
    let device = options.device_id.clone().map(|device| {
        let key = runner.next_id();
        lock(&runner.native.devices).insert(key, device);
        key
    });
    let login_options = json!({"agentName": options.agent_name, "deviceKey": device});
    let result = drive_login(&runner, interaction, cancel, |key, signal| async move {
        let args = vec![json!(kind), json!({"key": key, "signal": signal}), login_options];
        link.call("nativeLogin", args, &CancellationToken::new()).await.map_err(AuthError::Other)
    })
    .await;
    if let Some(device) = device {
        lock(&runner.native.devices).remove(&device);
    }
    result
}

/// `auth.oauth` of a provider object.
struct JsNativeOAuth {
    link: Link,
    name: String,
    subscription: bool,
    login_label: Option<String>,
}

#[async_trait]
impl OAuthAuth for JsNativeOAuth {
    fn name(&self) -> &str {
        &self.name
    }

    fn is_subscription(&self) -> bool {
        self.subscription
    }

    fn login_label(&self) -> Option<&str> {
        self.login_label.as_deref()
    }

    async fn login(
        &self,
        interaction: &dyn AuthInteraction,
        options: &LoginOptions,
        cancel: CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        oauth_credential(native_login(&self.link, "oauth", interaction, options, &cancel).await?)
    }

    async fn refresh(&self, credential: &OAuthCredential, cancel: CancellationToken) -> AuthResult<OAuthCredential> {
        let args = vec![json!("refresh"), json!(Credential::OAuth(credential.clone()))];
        oauth_credential(self.link.call("nativeOAuth", args, &cancel).await.map_err(AuthError::Other)?)
    }

    fn to_auth(&self, credential: &OAuthCredential) -> ModelAuth {
        let Ok(runner) = self.link.runner() else { return ModelAuth::default() };
        let args = [json!(self.link.target), json!(self.link.id), json!("toAuth")];
        let args = args.into_iter().chain([json!(Credential::OAuth(credential.clone())), Value::Null]);
        let call: Vec<HostValue> = match self.link.group {
            0 => args.map(HostValue::from).collect(),
            group => [json!(group), json!("nativeOAuth")].into_iter().chain(args).map(HostValue::from).collect(),
        };
        let method = if self.link.group == 0 { "nativeOAuth" } else { "groupCallback" };
        match runner.host().blocking_call_method_within(method, call, Duration::from_secs(10)) {
            Ok(value) => model_auth(&value.into_json()),
            Err(_) => ModelAuth::default(),
        }
    }

    async fn to_auth_async(&self, credential: &OAuthCredential) -> AuthResult<ModelAuth> {
        let args = vec![json!("toAuth"), json!(Credential::OAuth(credential.clone()))];
        let auth = self.link.call("nativeOAuth", args, &CancellationToken::new()).await.map_err(AuthError::Other)?;
        Ok(model_auth(&auth))
    }
}

/// Register the provider object described by `description` in `runtime`; JavaScript finds the
/// object under `target` in factory `group` of `callbacks`.
pub(super) fn install(
    runtime: &Arc<ModelRuntime>,
    callbacks: &Arc<ExtensionRunner>,
    group: u64,
    target: u64,
    id: &str,
    description: &Value,
) {
    let link = Link { runner: callbacks.weak(), group, target, id: id.to_string() };
    let text = |value: &Value| value.as_str().map(str::to_string);
    let oauth = description["oauth"].as_object().map(|oauth| {
        let name = oauth.get("name").and_then(Value::as_str).unwrap_or(id).to_string();
        let subscription = oauth.get("isSubscription").and_then(Value::as_bool) == Some(true);
        let login_label = oauth.get("loginLabel").and_then(Value::as_str).map(str::to_string);
        Arc::new(JsNativeOAuth { link: link.clone(), name, subscription, login_label }) as Arc<dyn OAuthAuth>
    });
    let api_key = &description["apiKey"];
    let auth = Arc::new(JsNativeAuth {
        link: link.clone(),
        api_key: api_key.is_object(),
        login: api_key["login"].as_bool() == Some(true),
        oauth: oauth.is_some(),
    });
    let stream =
        JsApiProvider { runner: callbacks.weak(), group, target: StreamTarget::Native { target, id: id.to_string() } };
    let info = NativeProviderInfo {
        name: text(&description["name"]).unwrap_or_else(|| id.to_string()),
        base_url: text(&description["baseUrl"]),
        refresh_models: description["refreshModels"].as_bool() == Some(true),
        refresh_with_configured_auth: description["refreshModelsAuth"] == "configured",
        filter_models: description["filterModels"].as_bool() == Some(true),
    };
    let provider = Arc::new(JsNativeProvider { link, stream, runtime: Arc::downgrade(runtime) });
    let models = description["models"].as_array().cloned().unwrap_or_default();
    runtime.register_native_provider(id, NativeRegistration { provider, info, models, auth, oauth });
}

impl ExtensionRunner {
    /// `pi.registerProvider(provider)` and `ctx.modelRegistry.registerProvider(provider)`.
    pub(crate) fn register_native_provider(&self, id: &str, description: &Value, path: &str) -> Result<(), String> {
        self.register_native_provider_for(&self.this(), 0, id, description, path)
    }

    /// Register with this runner's runtime; JavaScript callbacks go to factory `group` of
    /// `callbacks` (an SDK child host binding its parent's factories).
    pub(crate) fn register_native_provider_for(
        &self,
        callbacks: &Arc<Self>,
        group: u64,
        id: &str,
        description: &Value,
        path: &str,
    ) -> Result<(), String> {
        if id.trim().is_empty() {
            return Err("Provider id must not be empty.".into());
        }
        // A native registration replaces a `pi.registerProvider(name, config)` of the same id.
        self.unregister_provider(id);
        let runtime = self.model_runtime().clone();
        install(&runtime, callbacks, group, 0, id, description);
        lock(&self.providers).push((id.to_string(), path.to_string()));
        runtime.refresh_offline_in_background();
        if let Some(session) = self.session() {
            session.refresh_model_from_registry();
        }
        Ok(())
    }
}

/// `provider.*` sync operations of native providers.
pub(crate) fn sync(runner: &ExtensionRunner, name: &str, args: &Args) -> Result<Value, JsError> {
    Ok(match name {
        "registerNative" => {
            runner.register_native_provider(&args.str(0)?, &args.json(1), &args.str(2)?).map_err(error)?;
            Value::Null
        }
        // The models of a refreshing provider after its `publish` ran `update`.
        "nativeModels" => {
            let handle = lock(&runner.native.refreshes).get(&args.u64(0)).cloned();
            let (Some((runtime, id, generation)), Value::Array(models)) = (handle, args.json(1)) else {
                return Ok(json!(false));
            };
            let Some(runtime) = runtime.upgrade() else { return Ok(json!(false)) };
            let changed = runtime.set_native_models(&id, generation, &models);
            if let Some(session) = runner.session().filter(|session| Arc::ptr_eq(session.model_runtime(), &runtime)) {
                session.refresh_model_from_registry();
            }
            json!(changed)
        }
        "deviceId" => {
            let device = lock(&runner.native.devices).get(&args.u64(0)).cloned();
            json!(device.map(|device| device()))
        }
        other => return Err(error(format!("Unknown host operation: provider.{other}"))),
    })
}

/// `provider.publish(key, hasPersist, persist, signal)`: the persistence of a refresh's `publish`.
pub(crate) async fn publish(runner: &Arc<ExtensionRunner>, args: &Args) -> Result<Value, JsError> {
    let handle = lock(&runner.native.refreshes).get(&args.u64(0)).cloned();
    let Some((runtime, id, generation)) = handle else { return Ok(json!(false)) };
    let Some(runtime) = runtime.upgrade() else { return Ok(json!(false)) };
    let persist = args.bool(1).then(|| Some(args.json(2)).filter(|entry| !entry.is_null()));
    let cancel = runner.token(args.json(3).as_u64());
    Ok(json!(runtime.publish_native_models(&id, generation, persist, &cancel).await.map_err(error)?))
}
