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
mod tests_tools;
#[cfg(test)]
mod tests_transcript;
mod thinking;
mod tool_calls;

use self::compat::Compat;
use self::events::CompletionsStream;
use self::request::{BodyInput, build_body};
use crate::http::pi_user_agent;
use crate::providers::common::{HttpRequest, endpoint, has_header, header_map_with, spawn_provider_stream_with};
use crate::providers::constrained_sampling::grammar_tool_input_properties;
use crate::providers::github_copilot::copilot_dynamic_headers;
use crate::registry::{ApiProvider, CacheRetention, StreamOptions};
use crate::simple_options::resolve_cache_retention;
use crate::stream::{AssistantMessageEventStream, error_stream};
use crate::transcript::{declared_tools, normalize_context, resolve_transcript};
use crate::types::{Context, Message, Model, StopReason};

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
        let grammar = grammar_tool_input_properties(&declared_tools(&messages), compat.supports_openai_grammar_tools);
        let body = grammar.and_then(|grammar| {
            let input = BodyInput { messages: &messages, estimate: &normalized, grammar: &grammar };
            Ok((build_body(model, &input, &options, &compat)?, grammar))
        });
        let (body, grammar) = match body {
            Ok(built) => built,
            Err(error) => return error_stream(model, StopReason::Error, error),
        };
        let mut defaults = vec![("user-agent", pi_user_agent())];
        if let Some(key) = api_key {
            defaults.push(("authorization", format!("Bearer {key}")));
        }
        let overrides = header_overrides(model, &messages, &options, &compat);
        let request = HttpRequest {
            url: endpoint(&model.base_url, "/chat/completions"),
            headers: header_map_with(defaults, overrides, model, &options),
            body,
        };
        let supports_finish_reason = compat.supports_finish_reason;
        spawn_provider_stream_with(model, &options, request, |sender| {
            CompletionsStream::new(model, supports_finish_reason, grammar, sender)
        })
    }
}

/// Copilot's dynamic headers, then the session affinity headers. Upstream applies both over
/// `model.headers` and below the request headers.
fn header_overrides(
    model: &Model,
    messages: &[Message],
    options: &StreamOptions,
    compat: &Compat,
) -> Vec<(&'static str, String)> {
    let mut overrides = copilot_dynamic_headers(model, messages);
    let retention = resolve_cache_retention(options.cache_retention, &options.env);
    let session = options.session_id.as_ref().filter(|_| retention != CacheRetention::None);
    if let (Some(session), true) = (session, compat.send_session_affinity_headers) {
        let names: &[&'static str] = match compat.session_affinity_format.as_str() {
            "openrouter" => &["x-session-id"],
            "openai" => &["session_id", "x-client-request-id", "x-session-affinity"],
            _ => &["x-client-request-id", "x-session-affinity"],
        };
        overrides.extend(names.iter().map(|name| (*name, session.clone())));
    }
    overrides
}
