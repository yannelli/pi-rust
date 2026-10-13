//! Per-request credentials and provider settings. Every model request resolves auth through the
//! model runtime at send time, so OAuth tokens refresh and `!command` keys run when needed.

use std::sync::Arc;

use futures::StreamExt;
use pi_agent::StreamFn;
use pi_ai::auth::{AuthOverrides, Credential, ModelAuth, ProviderEnv};
use pi_ai::http::{HttpSettings, configure_http};
use pi_ai::{Model, StopReason, StreamOptions, ThinkingBudgets, ThinkingLevel, error_stream, event_channel};

use super::model_runtime::ModelRuntime;
use super::runtime_virtual::RouteOptions;
use crate::app::{format_no_api_key_found_message, oauth_failed_message};
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

/// Apply request auth, attribution headers, and the provider settings (`transport`,
/// `thinkingBudgets`, `retry.provider.*`, `httpIdleTimeoutMs`, `websocketConnectTimeoutMs`).
pub fn apply_request_settings(options: &mut StreamOptions, request: &RequestAuth, settings: &SettingsManager) {
    options.api_key = request.auth.api_key.clone();
    let mut headers = std::mem::take(&mut options.headers);
    headers.extend(request.auth.headers.clone());
    let telemetry = install_telemetry_enabled(settings);
    let merged = pi_ai::attribution::merge_provider_attribution_headers(
        &request.model,
        telemetry,
        options.session_id.as_deref(),
        &[Some(&headers)],
    );
    options.headers = merged.unwrap_or_default();
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
pub fn auth_stream_fn(runtime: Arc<ModelRuntime>, settings: Arc<SettingsManager>, inner: Option<StreamFn>) -> StreamFn {
    Arc::new(move |model: &Model, context: &pi_ai::Context, mut options: StreamOptions| {
        let (sender, stream) = event_channel();
        let (runtime, settings, inner) = (runtime.clone(), settings.clone(), inner.clone());
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
            let mut upstream = match request_auth(&runtime, &model).await {
                Ok(request) => {
                    apply_request_settings(&mut options, &request, &settings);
                    match &inner {
                        Some(stream_fn) => stream_fn(&request.model, &context, options),
                        None => pi_ai::stream(&request.model, &context, options),
                    }
                }
                Err(message) => error_stream(&model, StopReason::Error, message),
            };
            while let Some(event) = upstream.next().await {
                sender.push(event);
            }
        });
        stream
    })
}

/// Apply the global `httpProxy` and `httpIdleTimeoutMs` settings to every HTTP client.
pub fn configure_http_from_settings(settings: &SettingsManager) {
    configure_http(HttpSettings {
        http_proxy: settings.http_proxy(),
        idle_timeout_ms: settings.http_idle_timeout_ms().ok(),
    });
}
