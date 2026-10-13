//! Per-request credentials and provider settings: every model request resolves auth through
//! the [`AuthResolver`] (CLI key, `auth.json`, `models.json`, environment) at send time, so
//! OAuth tokens refresh and `!command` keys run when they are needed.

use std::sync::Arc;

use futures::StreamExt;
use pi_agent::StreamFn;
use pi_ai::auth::{AuthOverrides, AuthResolver, Credential, ModelAuth, ProviderEnv};
use pi_ai::{Model, StopReason, StreamOptions, ThinkingBudgets, error_stream, event_channel};

use super::setup::{format_no_api_key_found_message, oauth_failed_message};
use crate::settings::SettingsManager;

/// Auth for one request: the model with its resolved base URL, plus key, headers, and env.
pub(super) struct RequestAuth {
    pub model: Model,
    pub auth: ModelAuth,
    pub env: Option<ProviderEnv>,
}

/// Resolve request auth for `model`, failing with upstream messages when nothing usable is set.
pub(super) async fn request_auth(resolver: &AuthResolver, model: &Model) -> Result<RequestAuth, String> {
    let resolved = resolver
        .resolve_for_model(&model.provider, &model.id, &AuthOverrides::default())
        .await
        .map_err(|error| error.to_string())?;
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

/// Apply request auth and the provider settings (`transport`, `thinkingBudgets`, `retry.provider.*`,
/// `websocketConnectTimeoutMs`) to stream options.
pub(super) fn apply_request_settings(options: &mut StreamOptions, request: &RequestAuth, settings: &SettingsManager) {
    options.api_key = request.auth.api_key.clone();
    options.headers.extend(request.auth.headers.clone());
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
    options.timeout_ms = options.timeout_ms.or(provider.timeout_ms);
    options.max_retries = options.max_retries.or(provider.max_retries);
    options.max_retry_delay_ms = Some(provider.max_retry_delay_ms);
    if let Ok(Some(timeout)) = settings.websocket_connect_timeout_ms() {
        options.websocket_connect_timeout_ms = Some(timeout);
    }
}

/// Wrap `inner` (or `pi_ai::stream`) so each request carries resolved auth and settings.
pub(super) fn auth_stream_fn(
    resolver: Arc<AuthResolver>,
    settings: Arc<SettingsManager>,
    inner: Option<StreamFn>,
) -> StreamFn {
    Arc::new(move |model: &Model, context: &pi_ai::Context, mut options: StreamOptions| {
        let (sender, stream) = event_channel();
        let (resolver, settings, inner) = (resolver.clone(), settings.clone(), inner.clone());
        let (model, context) = (model.clone(), context.clone());
        tokio::spawn(async move {
            let mut upstream = match request_auth(&resolver, &model).await {
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
