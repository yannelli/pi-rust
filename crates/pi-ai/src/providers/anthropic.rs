//! Anthropic Messages (`POST {baseUrl}/v1/messages`) with SSE streaming. Later system messages
//! are sent in place when `compat.supportsMidConvoSystemMessages` is set, with native tool
//! changes when `compat.supportsMidConvoToolChanges` is also set.

mod convert;
mod events;
mod federation;
mod request;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_federation;
#[cfg(test)]
mod tests_strict_fallback;
#[cfg(test)]
mod tests_thinking;
#[cfg(test)]
mod tests_transcript;

use std::sync::Arc;

use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue};

use self::events::AnthropicStream;
pub(crate) use self::federation::OAUTH_API_BETA;
use self::federation::{FederationConfig, access_token, federation_config, invalidate};
use self::request::{
    Built, CLAUDE_CODE_VERSION, Compat, Params, Thinking, build_params, effort_messages, resolve_thinking,
};
use crate::http::pi_user_agent;
use crate::providers::common::{
    HttpRequest, RequestFallback, RequestRewrite, endpoint, has_header, header_map_with,
    spawn_provider_stream_with_fallback,
};
use crate::providers::github_copilot::{copilot_anthropic_auth_headers, copilot_dynamic_headers};
use crate::registry::{ApiProvider, CacheRetention, StreamOptions};
use crate::retry::ProviderRequestError;
use crate::simple_options::resolve_cache_retention;
use crate::stream::{AssistantMessageEvent, AssistantMessageEventStream, diagnostic, error_stream, event_channel};
use crate::transcript::{current_tools, normalize_context, resolve_transcript};
use crate::types::{Context, Message, Model, StopReason};

pub struct AnthropicMessages;

impl ApiProvider for AnthropicMessages {
    fn supports_system_messages(&self) -> bool {
        true
    }

    fn stream(&self, model: &Model, context: &Context, mut options: StreamOptions) -> AssistantMessageEventStream {
        if let Some(hook) = options.on_payload.take() {
            // A replaced payload still streams.
            options.on_payload = Some(Arc::new(move |payload, model| {
                let next = hook(payload, model);
                Box::pin(async move {
                    next.await.map(|mut next| {
                        if let Some(object) = next.as_object_mut() {
                            object.insert("stream".into(), true.into());
                        }
                        next
                    })
                })
            }));
        }
        let api_key = options.api_key.clone().filter(|key| !key.is_empty());
        let header_auth =
            ["authorization", "x-api-key", "cf-aig-authorization"].iter().any(|name| has_header(model, &options, name));
        let unauthenticated = api_key.is_none() && !header_auth;
        let federation =
            if unauthenticated && model.provider == "anthropic" { federation_config(&options.env) } else { None };
        if unauthenticated && federation.is_none() {
            return error_stream(model, StopReason::Error, format!("No API key for provider: {}", model.provider));
        }
        let compat = Compat::resolve(model);
        let messages = resolve_transcript(normalize_context(context), compat.mid_convo_system_messages);
        let copilot = model.provider == "github-copilot";
        let oauth = !copilot && api_key.as_deref().is_some_and(|key| key.contains("sk-ant-oat"));
        let thinking = resolve_thinking(model, &messages, &options, &compat);
        let params = Params {
            model,
            messages: &messages,
            options: &options,
            compat: &compat,
            thinking: &thinking,
            oauth,
            disable_preferred_strict_tools: false,
        };
        let Built { body, betas, preferred_strict_tools } = match build_params(&params) {
            Ok(built) => built,
            Err(error) => return error_stream(model, StopReason::Error, error),
        };
        let mut defaults = vec![("user-agent", pi_user_agent()), ("anthropic-version", "2023-06-01".to_string())];
        if copilot {
            defaults.extend(copilot_anthropic_auth_headers(api_key.as_deref()));
        } else {
            defaults.push(("accept", "application/json".to_string()));
            defaults.push(("anthropic-dangerous-direct-browser-access", "true".to_string()));
            match api_key {
                Some(key) if oauth => {
                    defaults.push(("authorization", format!("Bearer {key}")));
                    defaults.push(("user-agent", format!("claude-cli/{CLAUDE_CODE_VERSION}")));
                    defaults.push(("x-app", "cli".into()));
                }
                Some(key) => defaults.push(("x-api-key", key)),
                None => {}
            }
        }
        let retention = resolve_cache_retention(options.cache_retention, &options.env);
        if let Some(session) = options.session_id.as_ref().filter(|_| retention != CacheRetention::None)
            && !copilot
            && !oauth
            && compat.send_session_affinity_headers
        {
            let openrouter = compat.session_affinity_format.as_deref() == Some("openrouter");
            defaults.push((if openrouter { "x-session-id" } else { "x-session-affinity" }, session.clone()));
        }
        let mut headers = header_map_with(defaults, copilot_dynamic_headers(model, &messages), model, &options);
        if !betas.is_empty()
            && let Ok(value) = HeaderValue::from_str(&betas.join(","))
        {
            headers.insert("anthropic-beta", value);
        }
        let request = HttpRequest { url: endpoint(&model.base_url, "/v1/messages"), headers, body };
        let oauth_tools = if oauth { current_tools(&messages) } else { Vec::new() };
        let provider_thinking_level = effort_messages(model, &compat, &thinking)
            .then(|| thinking.effort.clone().unwrap_or_else(|| "high".into()));
        let handler_model = model.clone();
        let handler =
            move || AnthropicStream::new(&handler_model, oauth_tools.clone(), provider_thinking_level.clone());
        let fallback = preferred_strict_tools
            .then(|| strict_tool_fallback(model.clone(), messages, options.clone(), compat, thinking, oauth));
        match federation {
            Some(config) => federated_stream(model, options, request, fallback, config, handler),
            None => spawn_provider_stream_with_fallback(model, &options, request, fallback, |_| handler()),
        }
    }
}

/// Provider rejections of a strict tool set that is too large or too complex.
const STRICT_TOOL_LIMIT_MESSAGES: [&str; 3] = [
    "The compiled grammar is too large",
    "Too many strict tools (",
    "Schemas contains too many parameters with union types (",
];

/// A 400 `invalid_request_error` that names one of the strict tool limits.
fn rejects_strict_tool_limits(error: &ProviderRequestError) -> Option<String> {
    let body = error.body.as_deref()?;
    let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
    let rejected = error.status == Some(400)
        && parsed.pointer("/error/type").and_then(|kind| kind.as_str()) == Some("invalid_request_error")
        && STRICT_TOOL_LIMIT_MESSAGES.iter().any(|limit| body.contains(limit));
    rejected.then(|| parsed.pointer("/error/message").and_then(|m| m.as_str()).unwrap_or(&error.message).to_string())
}

/// Retries a rejected strict tool set once with `strict: "prefer"` tools using their original
/// non-strict schemas, keeping `strict: "require"` tools strict.
fn strict_tool_fallback(
    model: Model,
    messages: Vec<Message>,
    options: StreamOptions,
    compat: Compat,
    thinking: Thinking,
    oauth: bool,
) -> RequestFallback {
    Arc::new(move |error| {
        let message = rejects_strict_tool_limits(error)?;
        let params = Params {
            model: &model,
            messages: &messages,
            options: &options,
            compat: &compat,
            thinking: &thinking,
            oauth,
            disable_preferred_strict_tools: true,
        };
        let body = build_params(&params).ok()?.body;
        let details = serde_json::json!({"reason": "strict_tool_limits_exceeded"}).as_object().cloned();
        let diagnostic = diagnostic("anthropic_strict_tools_fallback", Some(&message), details);
        Some(RequestRewrite { body, diagnostic })
    })
}

/// Bearer auth from the federated access token, with the OAuth beta feature appended.
fn authorize(headers: &mut HeaderMap, token: &str) {
    if let Ok(value) = HeaderValue::from_str(&format!("Bearer {token}")) {
        headers.insert("authorization", value);
    }
    let existing = headers.get("anthropic-beta").and_then(|value| value.to_str().ok()).unwrap_or_default();
    let mut features: Vec<&str> = existing.split(',').map(str::trim).filter(|feature| !feature.is_empty()).collect();
    if !features.contains(&OAUTH_API_BETA) {
        features.push(OAUTH_API_BETA);
    }
    if let Ok(value) = HeaderValue::from_str(&features.join(",")) {
        headers.insert("anthropic-beta", value);
    }
}

/// Exchange the identity token, then stream. A 401 drops the cached token and sends once more.
fn federated_stream(
    model: &Model,
    options: StreamOptions,
    request: HttpRequest,
    fallback: Option<RequestFallback>,
    config: FederationConfig,
    handler: impl Fn() -> AnthropicStream + Send + 'static,
) -> AssistantMessageEventStream {
    let (sender, stream) = event_channel();
    let model = model.clone();
    tokio::spawn(async move {
        for retry in [false, true] {
            if retry {
                invalidate(&model.base_url, &config);
            }
            let mut inner = match access_token(&model.base_url, &config, &options.env, &options.cancel).await {
                Ok(token) => {
                    let mut request = request.clone();
                    authorize(&mut request.headers, &token);
                    spawn_provider_stream_with_fallback(&model, &options, request, fallback.clone(), |_| handler())
                }
                Err(error) if options.cancel.is_cancelled() => error_stream(&model, StopReason::Aborted, error),
                Err(error) => error_stream(&model, StopReason::Error, error),
            };
            let first = inner.next().await;
            let unauthorized = matches!(&first, Some(AssistantMessageEvent::Error { error, .. })
                if error.error_message.as_deref().is_some_and(|message| message.starts_with("Provider HTTP 401")));
            if unauthorized && !retry {
                continue;
            }
            if let Some(event) = first {
                sender.push(event);
            }
            while let Some(event) = inner.next().await {
                sender.push(event);
            }
            return;
        }
    });
    stream
}
