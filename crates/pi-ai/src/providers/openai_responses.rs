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

use self::request::{Compat, build_body, grammar_properties};
use crate::http::pi_user_agent;
use crate::providers::common::{HttpRequest, endpoint, has_header, header_map_with, spawn_provider_stream};
use crate::providers::github_copilot::copilot_dynamic_headers;
use crate::registry::{ApiProvider, CacheRetention, StreamOptions};
use crate::simple_options::resolve_cache_retention;
use crate::stream::{AssistantMessageEventStream, error_stream};
use crate::transcript::{normalize_context, resolve_transcript};
use crate::types::{Context, Model, StopReason};

pub(crate) use self::convert::{ConvertOptions, convert_messages};
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
        let built = grammar_properties(&messages, &compat)
            .and_then(|grammar| Ok((build_body(model, &messages, &options, &compat, &grammar)?, grammar)));
        let (body, grammar) = match built {
            Ok(built) => built,
            Err(error) => return error_stream(model, StopReason::Error, error),
        };
        let mut defaults = vec![("user-agent", pi_user_agent())];
        if let Some(key) = api_key {
            defaults.push(("authorization", format!("Bearer {key}")));
        }
        let mut overrides = copilot_dynamic_headers(model, &messages);
        let retention = resolve_cache_retention(options.cache_retention, &options.env);
        if let (Some(session), false) = (&options.session_id, retention == CacheRetention::None) {
            match compat.session_affinity_format.as_str() {
                "openrouter" => overrides.push(("x-session-id", session.clone())),
                format => {
                    if format == "openai" {
                        overrides.push(("session_id", session.clone()));
                    }
                    overrides.push(("x-client-request-id", session.clone()));
                }
            }
        }
        let request = HttpRequest {
            url: endpoint(&model.base_url, "/responses"),
            headers: header_map_with(defaults, overrides, model, &options),
            body,
        };
        let handler = ResponsesStream::new(model)
            .with_grammar_tool_input_properties(grammar)
            .with_service_tier_pricing(model, options.service_tier.clone());
        spawn_provider_stream(model, &options, request, handler)
    }
}
