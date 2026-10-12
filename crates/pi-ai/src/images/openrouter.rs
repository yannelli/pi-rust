//! Image generation over OpenRouter's chat completions endpoint (upstream `openrouter-images.ts`):
//! one non-streaming `POST {baseUrl}/chat/completions` with `modalities`.

use futures::future::BoxFuture;
use serde_json::{Value, json};

use super::{AssistantImages, ImagesApi, ImagesContext, ImagesOptions, ImagesStopReason};
use crate::classify::shared::{endpoint, error_message, failure};
use crate::request::{
    HttpRequest, MAX_PROVIDER_ERROR_BODY_CHARS, SendOptions, apply_payload_hook, build_headers, post_json,
    truncate_error_text,
};
use crate::retry::ProviderRequestError;
use crate::types::{ContentBlock, Cost, Model, Usage};

/// Image generation through OpenRouter's chat completions endpoint.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenRouterImages;

impl ImagesApi for OpenRouterImages {
    fn generate_images(
        &self,
        model: &Model,
        context: &ImagesContext,
        options: ImagesOptions,
    ) -> BoxFuture<'static, AssistantImages> {
        let (model, context) = (model.clone(), context.clone());
        Box::pin(async move {
            let mut output = AssistantImages::new(&model);
            if let Err(error) = run(&model, &context, &options, &mut output).await {
                output.stop_reason = match options.request.cancel.is_cancelled() {
                    true => ImagesStopReason::Aborted,
                    false => ImagesStopReason::Error,
                };
                output.error_message = Some(images_error_message(&error));
            }
            output
        })
    }
}

async fn run(
    model: &Model,
    context: &ImagesContext,
    options: &ImagesOptions,
    output: &mut AssistantImages,
) -> Result<(), ProviderRequestError> {
    let options = &options.request;
    let Some(api_key) = options.api_key.as_deref().filter(|key| !key.is_empty()) else {
        return Err(failure(format!("No API key for provider: {}", model.provider)));
    };
    let defaults = vec![
        ("authorization", format!("Bearer {api_key}")),
        ("content-type", "application/json".into()),
        ("accept", "application/json".into()),
    ];
    let headers = build_headers(defaults, &model.headers, &options.headers, &options.suppressed_headers);
    let body = apply_payload_hook(build_params(model, context), options.on_payload.as_ref(), model);
    let request = HttpRequest { url: endpoint(&model.base_url, "chat/completions"), headers, body };
    let send = SendOptions { label: Some("OpenRouter"), ..SendOptions::from_request(options, 0) };
    let (response, body) = post_json(&request, &send).await?;
    if let Some(on_response) = &options.on_response {
        on_response(&response, model);
    }
    parse_response(&body, model, output);
    Ok(())
}

fn build_params(model: &Model, context: &ImagesContext) -> Value {
    let content: Vec<Value> = context
        .input
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(json!({ "type": "text", "text": text })),
            ContentBlock::Image { data, mime_type } => {
                Some(json!({ "type": "image_url", "image_url": { "url": format!("data:{mime_type};base64,{data}") } }))
            }
            _ => None,
        })
        .collect();
    let with_text = model.output.as_ref().is_some_and(|output| output.iter().any(|modality| modality == "text"));
    let modalities = if with_text { json!(["image", "text"]) } else { json!(["image"]) };
    json!({
        "model": model.id,
        "messages": [{ "role": "user", "content": content }],
        "stream": false,
        "modalities": modalities,
    })
}

fn parse_response(body: &Value, model: &Model, output: &mut AssistantImages) {
    output.response_id = body.get("id").and_then(Value::as_str).map(String::from);
    if let Some(usage) = body.get("usage").filter(|usage| truthy(Some(usage))) {
        output.usage = Some(parse_usage(usage, model));
    }
    let Some(message) = body.pointer("/choices/0/message") else { return };
    if let Some(text) = message.get("content").and_then(Value::as_str).filter(|text| !text.is_empty()) {
        output.output.push(ContentBlock::text(text));
    }
    let images = message.get("images").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    for image in images {
        let url = image.get("image_url").and_then(|url| url.as_str().or_else(|| url.get("url")?.as_str()));
        if let Some((mime_type, data)) = url.and_then(parse_data_url) {
            output.output.push(ContentBlock::Image { data: data.into(), mime_type: mime_type.into() });
        }
    }
}

/// `^data:([^;]+);base64,(.+)$`, where `.` stops at line terminators.
fn parse_data_url(url: &str) -> Option<(&str, &str)> {
    let (mime_type, rest) = url.strip_prefix("data:")?.split_once(';')?;
    let data = rest.strip_prefix("base64,")?;
    let line_break = |character: char| matches!(character, '\n' | '\r' | '\u{2028}' | '\u{2029}');
    (!mime_type.is_empty() && !data.is_empty() && !data.contains(line_break)).then_some((mime_type, data))
}

/// JavaScript truthiness of an optional JSON value.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|number| number != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(_) => true,
    }
}

fn parse_usage(raw: &Value, model: &Model) -> Usage {
    let count = |value: Option<&Value>| value.and_then(Value::as_f64).unwrap_or(0.0);
    let details = raw.get("prompt_tokens_details");
    let prompt_tokens = count(raw.get("prompt_tokens"));
    let reported_cached = count(details.and_then(|details| details.get("cached_tokens")));
    let cache_write = count(details.and_then(|details| details.get("cache_write_tokens")));
    let cache_read = if cache_write > 0.0 { (reported_cached - cache_write).max(0.0) } else { reported_cached };
    let input = (prompt_tokens - cache_read - cache_write).max(0.0);
    let output = count(raw.get("completion_tokens"));
    let price = &model.cost;
    let mut cost = Cost {
        input: (price.input / 1_000_000.0) * input,
        output: (price.output / 1_000_000.0) * output,
        cache_read: (price.cache_read / 1_000_000.0) * cache_read,
        cache_write: (price.cache_write / 1_000_000.0) * cache_write,
        total: 0.0,
    };
    cost.total = cost.input + cost.output + cost.cache_read + cost.cache_write;
    Usage {
        input: input as u64,
        output: output as u64,
        cache_read: cache_read as u64,
        cache_write: cache_write as u64,
        total_tokens: (input + output + cache_read + cache_write) as u64,
        cost,
        ..Usage::default()
    }
}

fn images_error_message(error: &ProviderRequestError) -> String {
    match (error.status, &error.body) {
        (Some(status), Some(body)) if !error.aborted && !error.message.starts_with("Server requested ") => {
            sdk_error_message(status, body)
        }
        _ => error_message(error, None),
    }
}

/// The OpenAI SDK's message for a failed response, as upstream `formatProviderError` shows it:
/// `<status>: <error JSON>` when the `error` object adds detail, else `<status> <message>`.
fn sdk_error_message(status: u16, body: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let error = parsed.as_ref().and_then(|parsed| parsed.get("error"));
    let detail = match error {
        Some(error) if truthy(error.get("message")) => match &error["message"] {
            Value::String(message) => message.clone(),
            other => other.to_string(),
        },
        Some(error) if truthy(Some(error)) => error.to_string(),
        _ if parsed.is_none() => body.to_string(),
        _ => String::new(),
    };
    let message = match detail.is_empty() {
        true => format!("{status} status code (no body)"),
        false => format!("{status} {detail}"),
    };
    let error_body = error
        .filter(|error| error.as_object().is_some_and(|fields| !fields.is_empty()))
        .map(|error| truncate_error_text(error.to_string().trim(), MAX_PROVIDER_ERROR_BODY_CHARS));
    match error_body {
        Some(error_body) if !message.contains(&error_body) => format!("{status}: {error_body}"),
        _ => message,
    }
}
