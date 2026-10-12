//! Anthropic Messages (`POST {baseUrl}/v1/messages`) with SSE streaming. Later system messages
//! are sent in place when `compat.supportsMidConvoSystemMessages` is set, with native tool
//! changes when `compat.supportsMidConvoToolChanges` is also set.

mod convert;
mod events;
mod request;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_thinking;
#[cfg(test)]
mod tests_transcript;

use std::sync::Arc;

use self::events::AnthropicStream;
use self::request::{CLAUDE_CODE_VERSION, Compat, Params, build_params, resolve_thinking};
use crate::http::pi_user_agent;
use crate::providers::common::{HttpRequest, endpoint, has_header, header_map, spawn_provider_stream};
use crate::registry::{ApiProvider, CacheRetention, StreamOptions};
use crate::simple_options::resolve_cache_retention;
use crate::stream::{AssistantMessageEventStream, error_stream};
use crate::transcript::{current_tools, normalize_context, resolve_transcript};
use crate::types::{Context, Model, StopReason};

pub struct AnthropicMessages;

impl ApiProvider for AnthropicMessages {
    fn supports_system_messages(&self) -> bool {
        true
    }

    fn stream(&self, model: &Model, context: &Context, mut options: StreamOptions) -> AssistantMessageEventStream {
        if let Some(hook) = options.on_payload.take() {
            // A replaced payload still streams.
            options.on_payload = Some(Arc::new(move |payload, model| {
                hook(payload, model).map(|mut next| {
                    if let Some(object) = next.as_object_mut() {
                        object.insert("stream".into(), true.into());
                    }
                    next
                })
            }));
        }
        let api_key = options.api_key.clone().filter(|key| !key.is_empty());
        let header_auth =
            ["authorization", "x-api-key", "cf-aig-authorization"].iter().any(|name| has_header(model, &options, name));
        if api_key.is_none() && !header_auth {
            return error_stream(model, StopReason::Error, format!("No API key for provider: {}", model.provider));
        }
        let compat = Compat::resolve(model);
        let messages = resolve_transcript(normalize_context(context), compat.mid_convo_system_messages);
        let copilot = model.provider == "github-copilot";
        let oauth = !copilot && api_key.as_deref().is_some_and(|key| key.contains("sk-ant-oat"));
        let thinking = resolve_thinking(model, &messages, &options, &compat);
        let params =
            Params { model, messages: &messages, options: &options, compat: &compat, thinking: &thinking, oauth };
        let (body, betas) = build_params(&params);
        let mut defaults = vec![
            ("user-agent", pi_user_agent()),
            ("accept", "application/json".to_string()),
            ("anthropic-dangerous-direct-browser-access", "true".to_string()),
            ("anthropic-version", "2023-06-01".to_string()),
        ];
        match api_key {
            Some(key) if copilot || oauth => defaults.push(("authorization", format!("Bearer {key}"))),
            Some(key) => defaults.push(("x-api-key", key)),
            None => {}
        }
        if oauth {
            defaults.push(("user-agent", format!("claude-cli/{CLAUDE_CODE_VERSION}")));
            defaults.push(("x-app", "cli".into()));
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
        let mut headers = header_map(defaults, model, &options);
        if !betas.is_empty()
            && let Ok(value) = reqwest::header::HeaderValue::from_str(&betas.join(","))
        {
            headers.insert("anthropic-beta", value);
        }
        let request = HttpRequest { url: endpoint(&model.base_url, "/v1/messages"), headers, body };
        let oauth_tools = if oauth { current_tools(&messages) } else { Vec::new() };
        let provider_thinking_level = compat.mid_convo_effort.then(|| thinking.effort.unwrap_or_else(|| "high".into()));
        spawn_provider_stream(
            model,
            &options,
            request,
            AnthropicStream::new(model, oauth_tools, provider_thinking_level),
        )
    }
}
