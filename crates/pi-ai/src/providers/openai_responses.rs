//! OpenAI Responses (`POST {baseUrl}/responses`) with SSE streaming. Later system messages are
//! sent in place when `compat.supportsMidConvoSystemMessages` is set; tool additions anchor
//! as `additional_tools` or client tool search items when the model supports them.

mod convert;
mod events;
mod request;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_transcript;

use self::request::{Compat, build_body};
use crate::http::pi_user_agent;
use crate::providers::common::{HttpRequest, endpoint, has_header, header_map, spawn_provider_stream};
use crate::registry::{ApiProvider, CacheRetention, StreamOptions};
use crate::simple_options::resolve_cache_retention;
use crate::stream::{AssistantMessageEventStream, error_stream};
use crate::transcript::{normalize_context, resolve_transcript};
use crate::types::{Context, Model, StopReason};

// Shared with the Codex and Copilot adapters.
#[allow(unused_imports)]
pub(crate) use self::convert::{ConvertOptions, TOOL_CALL_PROVIDERS, convert_messages, convert_tools};
pub(crate) use self::events::ResponsesStream;

pub struct OpenAIResponses;

impl ApiProvider for OpenAIResponses {
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
        let messages = resolve_transcript(normalize_context(context), compat.supports_mid_convo_system_messages);
        let body = build_body(model, &messages, &options, &compat);
        let mut defaults = vec![("user-agent", pi_user_agent())];
        if let Some(key) = api_key {
            defaults.push(("authorization", format!("Bearer {key}")));
        }
        let retention = resolve_cache_retention(options.cache_retention, &options.env);
        if let (Some(session), false) = (&options.session_id, retention == CacheRetention::None) {
            match compat.session_affinity_format.as_str() {
                "openrouter" => defaults.push(("x-session-id", session.clone())),
                format => {
                    if format == "openai" {
                        defaults.push(("session_id", session.clone()));
                    }
                    defaults.push(("x-client-request-id", session.clone()));
                }
            }
        }
        let request = HttpRequest {
            url: endpoint(&model.base_url, "/responses"),
            headers: header_map(defaults, model, &options),
            body,
        };
        spawn_provider_stream(model, &options, request, ResponsesStream::new(model))
    }
}
