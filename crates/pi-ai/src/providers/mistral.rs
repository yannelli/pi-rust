//! Mistral chat completions (`POST {baseUrl}/v1/chat/completions`) with SSE streaming,
//! the `mistral-conversations` API.

mod request;
mod stream;
#[cfg(test)]
mod tests;

use self::request::{build_payload, prompt_cache_session, to_wire};
use self::stream::MistralStream;
use crate::http::pi_user_agent;
use crate::providers::common::{HttpRequest, endpoint, header_map};
use crate::providers::http_stream::{ResponseReport, SendConfig, spawn_builder_stream, status_text};
use crate::registry::{ApiProvider, StreamOptions};
use crate::request::truncate_error_text;
use crate::retry::ProviderRequestError;
use crate::stream::{AssistantMessageEventStream, error_stream};
use crate::types::{Context, Model, StopReason};

const MAX_ERROR_BODY_CHARS: usize = 4000;
/// Default wait for response headers; the body may stream for longer.
const DEFAULT_HEADER_TIMEOUT_MS: u64 = 60_000;

pub struct MistralConversations;

impl ApiProvider for MistralConversations {
    fn supports_system_messages(&self) -> bool {
        true
    }

    fn stream(&self, model: &Model, context: &Context, options: StreamOptions) -> AssistantMessageEventStream {
        let Some(api_key) = options.api_key.clone().filter(|key| !key.is_empty()) else {
            return error_stream(model, StopReason::Error, format!("No API key for provider: {}", model.provider));
        };
        let payload = match build_payload(model, context, &options) {
            Ok(payload) => payload,
            Err(error) => return error_stream(model, StopReason::Error, error),
        };
        let mut defaults = vec![
            ("user-agent", pi_user_agent()),
            ("accept", "text/event-stream".to_string()),
            ("authorization", format!("Bearer {api_key}")),
        ];
        if let Some(session) = prompt_cache_session(&options) {
            defaults.push(("x-affinity", session));
        }
        let request = HttpRequest {
            url: endpoint(&model.base_url, "/v1/chat/completions"),
            headers: header_map(defaults, model, &options),
            body: payload,
        };
        let timeout = options.timeout_ms.unwrap_or(DEFAULT_HEADER_TIMEOUT_MS);
        let config = SendConfig {
            max_retries: 0,
            timeout: Some((timeout, format!("Mistral response headers timed out after {timeout}ms"))),
            report: ResponseReport::Always,
        };
        let handler = MistralStream::new(model.cost.clone());
        spawn_builder_stream(model, options, request, to_wire, config, mistral_error, handler)
    }
}

/// `Mistral API error (<status>): <body>`, as upstream formats HTTP failures.
fn mistral_error(error: ProviderRequestError) -> String {
    let Some(status) = error.status else { return error.message };
    match error.body.as_deref().map(str::trim).filter(|body| !body.is_empty()) {
        Some(body) => format!("Mistral API error ({status}): {}", truncate_error_text(body, MAX_ERROR_BODY_CHARS)),
        None => {
            let reason = match status_text(status) {
                "" => format!("Request failed with status {status}"),
                text => text.to_string(),
            };
            format!("Mistral API error ({status}): {reason}")
        }
    }
}
