//! OpenAI Chat Completions (`POST {baseUrl}/chat/completions`) with SSE streaming, including
//! the OpenAI-compatible server variants. Later system messages are sent in place when
//! `compat.supportsMidConvoSystemMessages` is set; Kimi-style tool additions anchor in system messages.

mod compat;
mod convert;
mod events;
mod reasoning_details;
mod request;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_cache;
#[cfg(test)]
mod tests_request;
#[cfg(test)]
mod tests_stream;
#[cfg(test)]
mod tests_thinking;
#[cfg(test)]
mod tests_transcript;
mod thinking;

use std::collections::BTreeMap;

use self::compat::Compat;
use self::events::CompletionsStream;
use self::request::build_body;
use crate::http::pi_user_agent;
use crate::providers::common::{HttpRequest, endpoint, has_header, spawn_provider_stream_with};
use crate::registry::{ApiProvider, CacheRetention, StreamOptions};
use crate::request::build_headers;
use crate::simple_options::resolve_cache_retention;
use crate::stream::{AssistantMessageEventStream, error_stream};
use crate::transcript::{normalize_context, resolve_transcript};
use crate::types::{Context, Model, StopReason};

pub struct OpenAICompletions;

impl ApiProvider for OpenAICompletions {
    fn supports_system_messages(&self) -> bool {
        true
    }

    fn stream(&self, model: &Model, context: &Context, options: StreamOptions) -> AssistantMessageEventStream {
        let api_key = options.api_key.clone().filter(|key| !key.is_empty());
        let header_auth =
            ["authorization", "cf-aig-authorization"].iter().any(|name| has_header(model, &options, name));
        if api_key.is_none() && !header_auth {
            return error_stream(model, StopReason::Error, format!("No API key for provider: {}", model.provider));
        }
        let compat = Compat::resolve(model);
        let normalized = normalize_context(context);
        let messages = resolve_transcript(normalized.clone(), compat.supports_mid_convo_system_messages);
        let body = build_body(model, &messages, &normalized, &options, &compat);
        let mut defaults = vec![("user-agent", pi_user_agent())];
        if let Some(key) = api_key {
            defaults.push(("authorization", format!("Bearer {key}")));
        }
        let request = HttpRequest {
            url: endpoint(&model.base_url, "/chat/completions"),
            headers: build_headers(
                defaults,
                &model_headers(model, &options, &compat),
                &options.headers,
                &options.suppressed_headers,
            ),
            body,
        };
        let supports_finish_reason = compat.supports_finish_reason;
        spawn_provider_stream_with(model, &options, request, |sender| {
            CompletionsStream::new(model, supports_finish_reason, sender)
        })
    }
}

/// `model.headers` with the session affinity headers applied over them, as upstream merges
/// them before the request headers.
fn model_headers(model: &Model, options: &StreamOptions, compat: &Compat) -> BTreeMap<String, String> {
    let mut headers = model.headers.clone();
    let retention = resolve_cache_retention(options.cache_retention, &options.env);
    let session = options.session_id.as_ref().filter(|_| retention != CacheRetention::None);
    let (Some(session), true) = (session, compat.send_session_affinity_headers) else { return headers };
    let names: &[&str] = match compat.session_affinity_format.as_str() {
        "openrouter" => &["x-session-id"],
        "openai" => &["session_id", "x-client-request-id", "x-session-affinity"],
        _ => &["x-client-request-id", "x-session-affinity"],
    };
    for name in names {
        headers.retain(|key, _| !key.eq_ignore_ascii_case(name));
        headers.insert(name.to_string(), session.clone());
    }
    headers
}
