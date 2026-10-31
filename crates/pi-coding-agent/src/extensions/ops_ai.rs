//! Session-free `ai.*` operations behind `@earendil-works/pi-ai/providers/all`: classifiers, image
//! generation, built-in API key auth, and the built-in OAuth flows with a JavaScript interaction.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use pi_ai::auth::oauth::{builtin_oauth, radius_oauth};
use pi_ai::auth::{
    AuthError, AuthEvent, AuthInteraction, AuthOverrides, AuthPrompt, AuthResolver, AuthResult,
    InMemoryCredentialStore, LoginOptions, OAuthAuth, OAuthCredential,
};
use pi_ai::env::EnvLookup;
use pi_ai::request::RequestOptions;
use pi_js::JsError;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::ops::{Args, error};
use super::runner::ExtensionRunner;

fn json_arg<T: serde::de::DeserializeOwned>(args: &Args, index: usize, what: &str) -> Result<T, JsError> {
    serde_json::from_value(args.json(index)).map_err(|failure| error(format!("Invalid {what}: {failure}")))
}

/// Request options of upstream `ProviderRequestOptions`: auth, provider env, and the signal.
fn request_options(runner: &ExtensionRunner, options: &Value) -> RequestOptions {
    let mut request = RequestOptions {
        api_key: options["apiKey"].as_str().map(str::to_string),
        cancel: runner.token(options["signal"].as_u64()),
        env: serde_json::from_value(options["env"].clone()).unwrap_or_default(),
        ..RequestOptions::default()
    };
    for (name, header) in options["headers"].as_object().into_iter().flatten() {
        match header.as_str() {
            Some(text) => drop(request.headers.insert(name.clone(), text.to_string())),
            None if header.is_null() => request.suppressed_headers.push(name.clone()),
            None => {}
        }
    }
    request
}

/// The built-in OAuth flow named by `{ provider, name?, gateway? }`; a gateway selects Radius.
fn oauth_for(spec: &Value) -> Result<Arc<dyn OAuthAuth>, JsError> {
    let provider = spec["provider"].as_str().unwrap_or_default();
    if let Some(gateway) = spec["gateway"].as_str() {
        return Ok(radius_oauth(spec["name"].as_str().unwrap_or("Radius"), gateway));
    }
    builtin_oauth(provider).ok_or_else(|| error(format!("Provider {provider} has no built-in OAuth login")))
}

/// Upstream `AuthInteraction` of the caller: prompts and events go to JavaScript callback `key`.
struct JsInteraction {
    runner: Arc<ExtensionRunner>,
    key: u64,
}

#[async_trait]
impl AuthInteraction for JsInteraction {
    async fn prompt(&self, prompt: AuthPrompt, cancel: CancellationToken) -> AuthResult<String> {
        let call = self.runner.call_json("invokeCallback", vec![json!(self.key), json!("prompt"), json!(prompt)]);
        let answer = tokio::select! {
            answer = call => answer.map_err(|failure| AuthError::other(failure.message))?,
            () = cancel.cancelled() => return Err(AuthError::Cancelled),
        };
        Ok(answer.as_str().map(str::to_string).unwrap_or_default())
    }

    fn notify(&self, event: AuthEvent) {
        let host = self.runner.host().clone();
        let args = vec![json!(self.key).into(), json!("notify").into(), json!(event).into()];
        tokio::spawn(async move {
            let _ = host.call_method("invokeCallback", args).await;
        });
    }
}

async fn oauth(runner: &Arc<ExtensionRunner>, args: &Args) -> Result<Value, JsError> {
    let (method, oauth) = (args.str(0)?, oauth_for(&args.json(1))?);
    let failed = |failure: AuthError| error(failure.to_string());
    Ok(match method.as_str() {
        "refresh" => {
            let credential: OAuthCredential = json_arg(args, 2, "OAuth credential")?;
            json!(oauth.refresh(&credential, runner.token(args.json(3).as_u64())).await.map_err(failed)?)
        }
        "toAuth" => json!(oauth.to_auth(&json_arg(args, 2, "OAuth credential")?)),
        "login" => {
            let interaction = JsInteraction { runner: runner.clone(), key: args.u64(2) };
            let options = args.json(3);
            let login =
                LoginOptions { agent_name: options["agentName"].as_str().map(str::to_string), ..Default::default() };
            let cancel = runner.token(options["signal"].as_u64());
            json!(oauth.login(&interaction, &login, cancel).await.map_err(failed)?)
        }
        other => return Err(error(format!("Unknown OAuth method: {other}"))),
    })
}

/// What the caller's `AuthContext` reported: credential variables and `fileExists` answers.
/// Paths it was not asked about count as missing.
#[derive(serde::Deserialize, Default)]
struct ContextEnv {
    #[serde(default)]
    vars: BTreeMap<String, String>,
    #[serde(default)]
    files: BTreeMap<String, bool>,
}

impl EnvLookup for ContextEnv {
    fn var(&self, name: &str) -> Option<String> {
        self.vars.get(name).cloned()
    }

    fn file_exists(&self, path: &str) -> bool {
        self.files.get(path).copied().unwrap_or(false)
    }
}

/// Upstream's built-in `ApiKeyAuth.resolve`: a stored key, else what the caller's `AuthContext`
/// reported, never the host environment or file system.
async fn api_key(args: &Args) -> Result<Value, JsError> {
    let provider = args.str(0)?;
    let credential = args.json(1);
    let overrides = AuthOverrides {
        api_key: credential["key"].as_str().map(str::to_string),
        env: serde_json::from_value(credential["env"].clone()).ok(),
        min_oauth_validity_ms: None,
    };
    let context: ContextEnv = serde_json::from_value(args.json(2)).unwrap_or_default();
    let store = Arc::new(InMemoryCredentialStore::new(BTreeMap::new()));
    let resolver = AuthResolver::without_oauth(store).with_env(Arc::new(context));
    let resolved = resolver.resolve(&provider, &overrides).await.map_err(|failure| error(failure.to_string()))?;
    Ok(resolved
        .map_or(Value::Null, |resolved| json!({"auth": resolved.auth, "env": resolved.env, "source": resolved.source})))
}

/// `ai.<name>`; `classify` and `generateImages` take the model, the context, and the options.
pub(crate) async fn run(runner: &Arc<ExtensionRunner>, name: &str, args: Args) -> Result<Value, JsError> {
    Ok(match name {
        "classify" => {
            let options_json = args.json(2);
            let options = pi_ai::classify::ClassifierOptions {
                request: request_options(runner, &options_json),
                temperature: options_json["temperature"].as_f64(),
            };
            let (model, context) = (json_arg(&args, 0, "model")?, json_arg(&args, 1, "classifier context")?);
            json!(pi_ai::classify::classify(&model, &context, options).await)
        }
        "generateImages" => {
            let options_json = args.json(2);
            let options = pi_ai::images::ImagesOptions {
                request: request_options(runner, &options_json),
                metadata: options_json["metadata"].as_object().cloned(),
            };
            let (model, context) = (json_arg(&args, 0, "model")?, json_arg(&args, 1, "images context")?);
            json!(pi_ai::images::generate_images(&model, &context, options).await)
        }
        "apiKey" => api_key(&args).await?,
        "oauth" => oauth(runner, &args).await?,
        other => return Err(error(format!("Unknown host operation: ai.{other}"))),
    })
}
