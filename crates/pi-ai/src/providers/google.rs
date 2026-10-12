//! Google Generative AI (`POST {baseUrl}/models/{id}:streamGenerateContent?alt=sse`), the
//! Gemini API wire protocol of `@google/genai`. Shared request and stream code also serves Vertex.

pub(crate) mod convert;
mod stream;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_convert;
pub(crate) mod thinking;

use std::future::Future;

use futures::StreamExt;
use serde_json::{Map, Value, json};

use self::convert::{convert_messages, convert_tools, function_calling_mode, supports_strict_tool_sampling};
use self::stream::GoogleStream;
use self::thinking::thinking_config;
use crate::http::pi_user_agent;
use crate::providers::common::{HttpRequest, header_map};
use crate::providers::emitter::Emitter;
use crate::providers::http_stream::{ResponseReport, SendConfig, hooked_events, send, status_text};
use crate::registry::{ApiProvider, StreamOptions};
use crate::request::apply_payload_hook;
use crate::retry::ProviderRequestError;
use crate::simple_options::clamp_max_tokens_to_context;
use crate::stream::{AssistantMessageEventStream, error_stream, event_channel};
use crate::types::{Context, Model, StopReason};

const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
/// `GenerateContentConfig` fields the SDK sends at the top level instead of in `generationConfig`.
const TOP_LEVEL_CONFIG: [&str; 7] =
    ["safetySettings", "tools", "toolConfig", "cachedContent", "labels", "serviceTier", "modelArmorConfig"];
/// Client-side `GenerateContentConfig` fields that never reach the wire.
const LOCAL_CONFIG: [&str; 3] = ["abortSignal", "httpOptions", "shouldReturnHttpResponse"];

pub struct GoogleGenerativeAI;

impl ApiProvider for GoogleGenerativeAI {
    fn stream(&self, model: &Model, context: &Context, options: StreamOptions) -> AssistantMessageEventStream {
        let Some(api_key) = options.api_key.clone().filter(|key| !key.is_empty()) else {
            return error_stream(model, StopReason::Error, format!("No API key for provider: {}", model.provider));
        };
        let params = match build_params(model, context, &options, false) {
            Ok(params) => params,
            Err(error) => return error_stream(model, StopReason::Error, error),
        };
        let base_url = match model.base_url.trim().trim_end_matches('/') {
            "" => DEFAULT_BASE_URL.to_string(),
            base => base.to_string(),
        };
        let prepare = {
            let (model, options) = (model.clone(), options.clone());
            async move {
                let params = apply_payload_hook(params, options.on_payload.as_ref(), &model);
                let (model_id, body) = to_wire(&params)?;
                let path = model_path(&model_id, false)?;
                let headers =
                    header_map(vec![("user-agent", pi_user_agent()), ("x-goog-api-key", api_key)], &model, &options);
                Ok(HttpRequest { url: format!("{base_url}/{path}:streamGenerateContent?alt=sse"), headers, body })
            }
        };
        spawn_google(model, options, "Google", prepare)
    }
}

/// SDK-shaped `GenerateContentParameters` (`{model, contents, config}`), as passed to `on_payload`.
pub(crate) fn build_params(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    vertex: bool,
) -> Result<Value, String> {
    let contents = convert_messages(model, context);
    let mut config = Map::new();
    if let Some(temperature) = options.temperature {
        config.insert("temperature".into(), json!(temperature));
    }
    let messages = crate::transcript::normalize_context(context);
    let max_tokens = clamp_max_tokens_to_context(model, &messages, options.max_tokens.unwrap_or(model.max_tokens));
    if max_tokens > 0 {
        config.insert("maxOutputTokens".into(), json!(max_tokens));
    }
    if let Some(system) = context.system_prompt.as_deref().filter(|system| !system.is_empty()) {
        config.insert("systemInstruction".into(), json!(system));
    }
    let strict = supports_strict_tool_sampling(&model.id);
    if !context.tools.is_empty() {
        let mode = function_calling_mode(&context.tools, options.tool_choice.as_deref(), strict)?;
        if let Some(tools) = convert_tools(&context.tools, false, strict)? {
            config.insert("tools".into(), tools);
        }
        if let Some(mode) = mode {
            config.insert("toolConfig".into(), json!({"functionCallingConfig": {"mode": mode}}));
        }
    }
    if let Some(thinking) = thinking_config(model, options, vertex)? {
        config.insert("thinkingConfig".into(), thinking);
    }
    Ok(json!({"model": model.id, "contents": contents, "config": config}))
}

/// The model id and request body the SDK would send for `params`.
pub(crate) fn to_wire(params: &Value) -> Result<(String, Value), String> {
    let model = params["model"].as_str().ok_or("model is required and must be a string")?.to_string();
    let mut body = Map::new();
    body.insert("contents".into(), params.get("contents").cloned().unwrap_or_else(|| json!([])));
    let mut generation = Map::new();
    for (key, value) in params["config"].as_object().into_iter().flatten() {
        if value.is_null() || LOCAL_CONFIG.contains(&key.as_str()) {
            continue;
        }
        if key == "systemInstruction" {
            body.insert(key.clone(), system_content(value));
        } else if TOP_LEVEL_CONFIG.contains(&key.as_str()) {
            body.insert(key.clone(), value.clone());
        } else {
            generation.insert(key.clone(), value.clone());
        }
    }
    if !generation.is_empty() {
        body.insert("generationConfig".into(), Value::Object(generation));
    }
    Ok((model, Value::Object(body)))
}

/// `tContent`: a string or parts become a user `Content`; a `Content` passes through.
fn system_content(value: &Value) -> Value {
    match value {
        Value::String(text) => json!({"role": "user", "parts": [{"text": text}]}),
        Value::Array(parts) => {
            let parts: Vec<Value> =
                parts.iter().map(|part| if part.is_string() { json!({"text": part}) } else { part.clone() }).collect();
            json!({"role": "user", "parts": parts})
        }
        Value::Object(object) if object.contains_key("parts") => value.clone(),
        other => json!({"role": "user", "parts": [other]}),
    }
}

/// Resource path of a model, as the SDK's `tModel` builds it.
pub(crate) fn model_path(model: &str, vertex: bool) -> Result<String, String> {
    if model.is_empty() {
        return Err("model is required and must be a string".into());
    }
    if model.contains("..") || model.contains('?') || model.contains('&') {
        return Err("invalid model parameter".into());
    }
    Ok(if vertex {
        if ["publishers/", "projects/", "models/"].iter().any(|prefix| model.starts_with(prefix)) {
            model.to_string()
        } else if let Some((publisher, name)) = model.split_once('/') {
            let name = name.split('/').next().unwrap_or(name);
            format!("publishers/{publisher}/models/{name}")
        } else {
            format!("publishers/google/models/{model}")
        }
    } else if model.starts_with("models/") || model.starts_with("tunedModels/") {
        model.to_string()
    } else {
        format!("models/{model}")
    })
}

/// The SDK error message: the JSON error body, or the text wrapped in an error object.
fn google_error(error: ProviderRequestError) -> String {
    let (Some(status), Some(body)) = (error.status, error.body.as_deref()) else { return error.message };
    let json = error.headers.get("content-type").is_some_and(|kind| kind.contains("application/json"));
    if json && let Ok(body) = serde_json::from_str::<Value>(body) {
        return body.to_string();
    }
    json!({"error": {"message": body, "code": status, "status": status_text(status)}}).to_string()
}

/// Run a prepared Gemini request and stream its chunks.
pub(crate) fn spawn_google<P>(
    model: &Model,
    options: StreamOptions,
    label: &'static str,
    prepare: P,
) -> AssistantMessageEventStream
where
    P: Future<Output = Result<HttpRequest, String>> + Send + 'static,
{
    let (sender, stream) = event_channel();
    let model = model.clone();
    tokio::spawn(async move {
        let mut out = Emitter::new(&model, sender);
        let cancel = options.cancel.clone();
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(String::new()),
            result = run(&mut out, &model, &options, label, prepare) => result,
        };
        match result {
            _ if cancel.is_cancelled() => out.finish(StopReason::Aborted, Some("Request was aborted".into())),
            Ok(reason) => out.finish(reason, None),
            Err(error) => out.finish(StopReason::Error, Some(error)),
        }
    });
    stream
}

async fn run<P>(
    out: &mut Emitter,
    model: &Model,
    options: &StreamOptions,
    label: &'static str,
    prepare: P,
) -> Result<StopReason, String>
where
    P: Future<Output = Result<HttpRequest, String>>,
{
    let request = prepare.await?;
    let config =
        SendConfig { max_retries: options.max_retries.unwrap_or(0), timeout: None, report: ResponseReport::Never };
    let response = send(&request, model, options, &config).await.map_err(google_error)?;
    out.start();
    let mut handler = GoogleStream::new(label);
    let mut events = std::pin::pin!(hooked_events(response, model, options));
    while let Some(event) = events.next().await {
        let event = event?;
        let data = event.data.trim();
        if data.is_empty() {
            continue;
        }
        let chunk: Value =
            serde_json::from_str(data).map_err(|error| format!("exception parsing stream chunk {data}. {error}"))?;
        handler.chunk(out, model, &chunk)?;
    }
    handler.end(out)
}
