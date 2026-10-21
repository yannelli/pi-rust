//! Per-request credentials and provider settings. Every model request resolves auth through the
//! model runtime at send time, so OAuth tokens refresh and `!command` keys run when needed.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::StreamExt;
use pi_agent::StreamFn;
use pi_ai::auth::{AuthOverrides, Credential, ModelAuth, ProviderEnv};
use pi_ai::http::{HttpSettings, configure_http};
use pi_ai::identity::opencode_session_header;
use pi_ai::{
    ClientIdentity, Model, StopReason, StreamOptions, ThinkingBudgets, ThinkingLevel, UserAgentMode, error_stream,
    event_channel, pi_attribution,
};

use super::model_runtime::ModelRuntime;
use super::runtime_virtual::RouteOptions;
use crate::app::{format_no_api_key_found_message, oauth_failed_message};
use crate::platform::VERSION;
use crate::settings::SettingsManager;
use crate::virtual_models::{ModelRouteReason, is_virtual_model};

/// Auth for one request: the model with its resolved base URL, plus key, headers, and env.
#[derive(Clone, Debug)]
pub struct RequestAuth {
    pub model: Model,
    pub auth: ModelAuth,
    pub env: Option<ProviderEnv>,
}

/// Resolve request auth for `model`, failing with upstream messages when nothing usable is set.
pub async fn request_auth(runtime: &ModelRuntime, model: &Model) -> Result<RequestAuth, String> {
    let resolver = runtime.resolver();
    let resolved = match runtime.resolve_provider_auth(&model.provider).await {
        Some(resolved) => resolved?,
        None => resolver
            .resolve_for_model(&model.provider, &model.id, &AuthOverrides::default())
            .await
            .map_err(|error| error.to_string())?,
    };
    match resolved {
        Some(resolved) if resolved.auth.api_key.is_some() || !resolved.auth.headers.is_empty() => {
            let mut model = model.clone();
            if let Some(base_url) = &resolved.auth.base_url {
                model.base_url = base_url.clone();
            }
            Ok(RequestAuth { model, auth: resolved.auth, env: resolved.env })
        }
        _ => {
            let stored = resolver.store().read(&model.provider).await.ok().flatten();
            Err(match stored {
                Some(Credential::OAuth(_)) => oauth_failed_message(&model.provider),
                _ => format_no_api_key_found_message(&model.provider),
            })
        }
    }
}

/// Install telemetry: `PI_TELEMETRY` when set, else the `enableInstallTelemetry` setting.
pub fn install_telemetry_enabled(settings: &SettingsManager) -> bool {
    match pi_ai::env::process_env_value("PI_TELEMETRY") {
        Some(value) => matches!(value.to_lowercase().as_str(), "1" | "true" | "yes"),
        None => settings.enable_install_telemetry(),
    }
}

/// How pi's requests identify it to providers (upstream `getClientIdentity`): a detailed
/// `User-Agent` with pi's version, and attribution to pi while install telemetry is enabled.
pub fn client_identity(settings: &SettingsManager) -> ClientIdentity {
    ClientIdentity {
        user_agent: Some(UserAgentMode::Detailed),
        version: Some(VERSION.to_string()),
        attribution: Some(install_telemetry_enabled(settings).then(pi_attribution)),
    }
}

/// Apply request auth, the client identity, and the provider settings (`transport`,
/// `thinkingBudgets`, `retry.provider.*`, `httpIdleTimeoutMs`, `websocketConnectTimeoutMs`).
pub fn apply_request_settings(options: &mut StreamOptions, request: &RequestAuth, settings: &SettingsManager) {
    options.api_key = request.auth.api_key.clone();
    // The OpenCode session header, then configured provider headers, then request headers, as
    // in upstream `ModelRuntime`.
    let session = opencode_session_header(&request.model, options.session_id.as_deref());
    let mut headers: BTreeMap<String, String> =
        session.map(|(name, value)| (name.to_string(), value)).into_iter().collect();
    for (name, value) in request.auth.headers.iter().chain(std::mem::take(&mut options.headers).iter()) {
        headers.retain(|existing, _| !existing.eq_ignore_ascii_case(name));
        headers.insert(name.clone(), value.clone());
    }
    options.headers = headers;
    // Replaces the agent loop's default identity; read per request so telemetry changes apply at once.
    options.identity = Some(client_identity(settings));
    if let Some(env) = &request.env {
        options.env.extend(env.clone());
    }
    options.transport = Some(settings.transport());
    if let Some(budgets) = settings.thinking_budgets() {
        options.thinking_budgets = Some(ThinkingBudgets {
            minimal: budgets.minimal,
            low: budgets.low,
            medium: budgets.medium,
            high: budgets.high,
        });
    }
    let provider = settings.provider_retry_settings();
    let idle = settings.http_idle_timeout_ms().ok().filter(|timeout| *timeout > 0);
    options.timeout_ms = options.timeout_ms.or(provider.timeout_ms).or(idle);
    options.max_retries = options.max_retries.or(provider.max_retries);
    options.max_retry_delay_ms = options.max_retry_delay_ms.or(Some(provider.max_retry_delay_ms));
    if let Ok(Some(timeout)) = settings.websocket_connect_timeout_ms() {
        options.websocket_connect_timeout_ms = options.websocket_connect_timeout_ms.or(Some(timeout));
    }
}

/// Route a request for a virtual model outside the agent loop (`direct`): the output budget is
/// capped to the routed model and the thinking level is the routed one.
async fn route_direct(
    runtime: &ModelRuntime,
    model: &Model,
    context: &pi_ai::Context,
    options: &mut StreamOptions,
) -> Result<Model, String> {
    let level = options.reasoning.unwrap_or(ThinkingLevel::Off);
    let mut route_options = RouteOptions::new(ModelRouteReason::Direct, level);
    route_options.cancel = options.cancel.clone();
    let route = runtime.resolve_model(model, context.messages.clone(), route_options).await;
    let route = route.map_err(|error| format!("{error:#}"))?;
    let limit = route.model.max_tokens;
    options.max_tokens = options.max_tokens.map(|tokens| if limit > 0 { tokens.min(limit) } else { tokens });
    options.reasoning = (route.thinking_level != ThinkingLevel::Off).then_some(route.thinking_level);
    // Keys of the virtual model's provider are not sent to another vendor.
    if route.model.provider != model.provider {
        options.api_key = None;
        options.headers.clear();
        options.env.clear();
    }
    Ok(route.model)
}

/// Wrap `inner` (or `pi_ai::stream`) so each request carries resolved auth and settings.
/// Virtual models are routed first, as requests outside the agent loop.
/// `hooks` adds the extension provider request hooks.
pub fn auth_stream_fn(
    runtime: Arc<ModelRuntime>,
    settings: Arc<SettingsManager>,
    inner: Option<StreamFn>,
    hooks: super::RequestHooksSlot,
) -> StreamFn {
    Arc::new(move |model: &Model, context: &pi_ai::Context, mut options: StreamOptions| {
        let (sender, stream) = event_channel();
        let (runtime, settings, inner, hooks) = (runtime.clone(), settings.clone(), inner.clone(), hooks.clone());
        let (mut model, context) = (model.clone(), context.clone());
        tokio::spawn(async move {
            if is_virtual_model(&model) {
                match route_direct(&runtime, &model, &context, &mut options).await {
                    Ok(routed) => model = routed,
                    Err(message) => {
                        let mut upstream = error_stream(&model, StopReason::Error, message);
                        while let Some(event) = upstream.next().await {
                            sender.push(event);
                        }
                        return;
                    }
                }
            }
            let mut observers = None;
            let mut upstream = match request_auth(&runtime, &model).await {
                Ok(request) => {
                    apply_request_settings(&mut options, &request, &settings);
                    super::extension_hooks::apply_header_hooks(&hooks, &mut options).await;
                    observers = super::extension_hooks::install_request_observers(&hooks, &mut options);
                    match &inner {
                        Some(stream_fn) => stream_fn(&request.model, &context, options),
                        None => pi_ai::stream(&request.model, &context, options),
                    }
                }
                Err(message) => error_stream(&model, StopReason::Error, message),
            };
            while let Some(event) = upstream.next().await {
                if let Some(observers) = &observers {
                    observers.settled().await;
                }
                sender.push(event);
            }
        });
        stream
    })
}

/// Apply the global `httpProxy` and `httpIdleTimeoutMs` settings to every HTTP client, and
/// export the proxy to child processes.
pub fn configure_http_from_settings(settings: &SettingsManager) {
    apply_http_proxy_env(settings.http_proxy().as_deref());
    configure_http(HttpSettings {
        http_proxy: settings.http_proxy(),
        idle_timeout_ms: settings.http_idle_timeout_ms().ok(),
    });
}

/// Upstream `applyHttpProxySettings`: `HTTP_PROXY` and `HTTPS_PROXY` for child processes,
/// unless the environment already sets them.
pub fn apply_http_proxy_env(proxy: Option<&str>) {
    let Some(proxy) = proxy.map(str::trim).filter(|proxy| !proxy.is_empty()) else { return };
    for name in ["HTTP_PROXY", "HTTPS_PROXY"] {
        if pi_ai::env::process_env_value(name).is_none() {
            pi_ai::env::set_child_process_env(name, proxy);
        }
    }
}

#[cfg(test)]
mod request_settings_tests {
    use pi_ai::identity::attribution_headers;
    use serde_json::json;

    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(name, value)| (name.to_string(), value.to_string())).collect()
    }

    fn captured(model: &Model, telemetry: bool, configured: &[(&str, &str)]) -> StreamOptions {
        let auth = ModelAuth { api_key: Some("test-api-key".into()), headers: headers(configured), base_url: None };
        let settings = SettingsManager::in_memory(json!({"enableInstallTelemetry": telemetry}));
        let mut options = StreamOptions { session_id: Some("s-1".into()), ..Default::default() };
        apply_request_settings(&mut options, &RequestAuth { model: model.clone(), auth, env: None }, &settings);
        options
    }

    fn model(provider: &str, base_url: &str) -> Model {
        Model { provider: provider.into(), base_url: base_url.into(), ..Model::default() }
    }

    #[test]
    fn http_proxy_setting_is_exported_to_child_processes() {
        let has_env = |name| std::env::var(name).is_ok();
        apply_http_proxy_env(Some("  "));
        apply_http_proxy_env(Some(" http://proxy.test:8080 "));
        let env = pi_ai::env::child_process_env();
        for name in ["HTTP_PROXY", "HTTPS_PROXY"] {
            if !has_env(name) {
                assert_eq!(env.get(name).map(String::as_str), Some("http://proxy.test:8080"));
            }
        }
        apply_http_proxy_env(Some("http://other.test"));
        assert_eq!(pi_ai::env::child_process_env().get("HTTP_PROXY"), env.get("HTTP_PROXY"));
    }

    /// Ported from upstream `sdk-client-identity.test.ts`.
    #[test]
    fn identifies_pi_and_attributes_requests_while_telemetry_is_enabled() {
        if std::env::var("PI_TELEMETRY").is_ok() {
            return;
        }
        let openrouter = model("openrouter", "https://openrouter.ai/api/v1");
        let identity = captured(&openrouter, true, &[]).identity;
        let expected = ClientIdentity {
            user_agent: Some(UserAgentMode::Detailed),
            version: Some(VERSION.into()),
            attribution: Some(Some(pi_attribution())),
        };
        assert_eq!(identity.as_ref(), Some(&expected));
        let names: Vec<_> = attribution_headers(&openrouter, identity.as_ref()).into_iter().map(|h| h.0).collect();
        assert_eq!(names, ["HTTP-Referer", "X-OpenRouter-Title", "X-OpenRouter-Categories"]);
        let nim = model("nvidia", "https://integrate.api.nvidia.com/v1");
        let off = captured(&nim, false, &[]).identity;
        assert_eq!(off, Some(ClientIdentity { attribution: Some(None), ..expected }));
        assert!(attribution_headers(&nim, off.as_ref()).is_empty());
    }

    #[test]
    fn adds_the_opencode_session_header_below_configured_headers() {
        let custom = captured(&model("custom-opencode", "https://opencode.ai/zen/v1"), false, &[]);
        assert_eq!(custom.headers, headers(&[("x-opencode-session", "s-1")]));
        let configured = [("X-OpenCode-Session", "configured-session")];
        let opencode = captured(&model("opencode", "https://opencode.ai/zen/v1"), false, &configured);
        assert_eq!(opencode.headers, headers(&configured));
        assert!(captured(&model("openai", "https://api.openai.com/v1"), false, &[]).headers.is_empty());
    }
}
