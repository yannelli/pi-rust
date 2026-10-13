//! Amazon Bedrock ConverseStream (`POST {endpoint}/model/{modelId}/converse-stream`), signed
//! with SigV4 or sent with a Bedrock API key, streaming AWS event-stream frames.

mod events;
mod model_info;
mod request;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use futures::StreamExt;
use reqwest::header::HeaderMap;
use serde_json::Value;

use self::events::{BedrockEvents, Failure, error_prefix, with_retention_hint};
use crate::http::client_for_env;
use crate::providers::aws::credentials::{self, ChainInput, profile_region};
use crate::providers::aws::endpoint::{regional_endpoint, standard_bedrock_endpoint_region};
use crate::providers::aws::event_stream::Decoder;
use crate::providers::aws::sigv4::{Credentials, SignableRequest, sign, uri_encode};
use crate::providers::provider_env::{process_env_value, provider_env_value};
use crate::registry::{ApiProvider, StreamOptions};
use crate::request::provider_response;
use crate::stream::{AssistantMessageEventStream, MessageBuilder, event_channel};
use crate::types::{Context, Model, StopReason};

/// Sends per request, as the AWS SDK does for throttling and transient failures.
const MAX_SENDS: u32 = 3;

pub struct BedrockConverseStream;

impl ApiProvider for BedrockConverseStream {
    fn stream(&self, model: &Model, context: &Context, options: StreamOptions) -> AssistantMessageEventStream {
        let (sender, stream) = event_channel();
        let mut builder = MessageBuilder::new(model, sender);
        let (model, context) = (model.clone(), context.clone());
        tokio::spawn(async move {
            let cancel = options.cancel.clone();
            let mut run = Run { events: BedrockEvents::new(model.cost.clone()), request_id: None };
            let result = tokio::select! {
                biased;
                _ = cancel.cancelled() => Err(Failure::new("Request was aborted")),
                result = run.execute(&model, &context, &options, &mut builder) => result,
            };
            run.events.finalize(&mut builder);
            if cancel.is_cancelled() {
                return builder.fail(StopReason::Aborted, "Request was aborted");
            }
            match result {
                Ok(reason) => builder.finish(reason),
                Err(failure) => {
                    if let Some(diagnostic) = failure.diagnostic(run.request_id.as_deref()) {
                        builder.message_mut().diagnostics.push(diagnostic);
                    }
                    builder.fail(StopReason::Error, failure.message);
                }
            }
        });
        stream
    }
}

/// Where and how a request is sent, resolved like upstream's Bedrock client config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Target {
    pub endpoint: String,
    pub region: String,
    /// `AWS_REGION` or `AWS_DEFAULT_REGION`, which also select GovCloud request fields.
    pub configured_region: Option<String>,
    /// A profile chosen for this request through `options.env`, which beats ambient keys.
    pub profile: Option<String>,
}

fn arn_region(model_id: &str) -> Option<&str> {
    let mut parts = model_id.split(':');
    let (arn, partition, service, region) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    let valid_partition = partition == "aws" || partition.starts_with("aws-");
    let valid_region =
        !region.is_empty() && region.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    (arn == "arn" && valid_partition && service == "bedrock" && valid_region && parts.next().is_some())
        .then_some(region)
}

/// Region: ARN-embedded, then configured, then a pinned standard endpoint's, then the
/// profile's config region when an ambient profile is set, else `us-east-1`.
pub(crate) fn resolve_target(model: &Model, env: &BTreeMap<String, String>) -> Result<Target, String> {
    let profile = env.get("AWS_PROFILE").filter(|profile| !profile.is_empty()).cloned();
    let configured_region =
        provider_env_value("AWS_REGION", env).or_else(|| provider_env_value("AWS_DEFAULT_REGION", env));
    let ambient_profile = process_env_value("AWS_PROFILE");
    let endpoint_region = standard_bedrock_endpoint_region(&model.base_url);
    let explicit = !model.base_url.is_empty()
        && (endpoint_region.is_none() || (configured_region.is_none() && ambient_profile.is_none()));
    let region = match (arn_region(&model.id), &configured_region, &endpoint_region) {
        (Some(region), _, _) => region.to_string(),
        (None, Some(region), _) => region.clone(),
        (None, None, Some(region)) if explicit => region.clone(),
        _ if ambient_profile.is_none() => "us-east-1".to_string(),
        _ => {
            let name = profile.clone().or(ambient_profile).unwrap_or_default();
            profile_region(&name, env).ok_or_else(|| {
                format!("Region is missing for AWS profile {name}. Set AWS_REGION or a profile region.")
            })?
        }
    };
    let endpoint = if explicit {
        model.base_url.clone()
    } else if let Some(url) = provider_env_value("AWS_ENDPOINT_URL_BEDROCK_RUNTIME", env)
        .or_else(|| provider_env_value("AWS_ENDPOINT_URL", env))
    {
        url
    } else {
        let flag = |name: &str| provider_env_value(name, env).is_some_and(|value| value.eq_ignore_ascii_case("true"));
        regional_endpoint("bedrock-runtime", &region, flag("AWS_USE_FIPS_ENDPOINT"), flag("AWS_USE_DUALSTACK_ENDPOINT"))
    };
    Ok(Target { endpoint, region, configured_region, profile })
}

enum Auth {
    Bearer(String),
    Sign(Credentials),
}

async fn resolve_auth(options: &StreamOptions, target: &Target) -> Result<Auth, String> {
    let skip_auth = provider_env_value("AWS_BEDROCK_SKIP_AUTH", &options.env).as_deref() == Some("1");
    let bearer = options
        .api_key
        .clone()
        .filter(|key| !key.is_empty())
        .or_else(|| provider_env_value("AWS_BEARER_TOKEN_BEDROCK", &options.env));
    match (skip_auth, bearer) {
        (true, _) => Ok(Auth::Sign(Credentials::new("dummy-access-key", "dummy-secret-key"))),
        (false, Some(token)) => Ok(Auth::Bearer(token)),
        (false, None) => {
            let input = ChainInput {
                env: &options.env,
                profile: target.profile.clone(),
                region: target.region.clone(),
                mfa_code_provider: None,
            };
            credentials::resolve(&input).await.map(Auth::Sign)
        }
    }
}

/// `host`, `authorization`, and `x-amz-*` belong to SigV4 or the bearer token.
fn is_reserved_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("x-amz-") || lower == "authorization" || lower == "host"
}

struct Run {
    events: BedrockEvents,
    request_id: Option<String>,
}

impl Run {
    async fn execute(
        &mut self,
        model: &Model,
        context: &Context,
        options: &StreamOptions,
        builder: &mut MessageBuilder,
    ) -> Result<StopReason, Failure> {
        let target = resolve_target(model, &options.env).map_err(Failure::new)?;
        let mut body =
            request::build_body(model, context, options, target.configured_region.as_deref()).map_err(Failure::new)?;
        if let Some(replacement) = options.on_payload.as_ref().and_then(|hook| hook(&body, model)) {
            body = replacement;
        }
        let model_id = body.get("modelId").and_then(Value::as_str).unwrap_or(&model.id).to_string();
        if let Some(object) = body.as_object_mut() {
            object.remove("modelId");
        }
        let url = format!("{}/model/{}/converse-stream", target.endpoint.trim_end_matches('/'), uri_encode(&model_id));
        let url = reqwest::Url::parse(&url)
            .map_err(|error| Failure::new(format!("Invalid Bedrock endpoint {url}: {error}")))?;
        let payload = serde_json::to_vec(&body).unwrap_or_default();
        let mut headers = vec![("content-type".to_string(), "application/json".to_string())];
        let custom = options.headers.iter().filter(|(name, _)| !is_reserved_header(name));
        headers.extend(custom.map(|(name, value)| (name.clone(), value.clone())));
        let auth = resolve_auth(options, &target).await.map_err(Failure::new)?;
        let response = self.send(&url, &headers, &payload, &auth, &target, options).await?;
        if let Some(hook) = &options.on_response {
            hook(&provider_response(&response), model);
        }
        let mut decoder = Decoder::default();
        let mut chunks = response.bytes_stream();
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.map_err(|error| Failure::new(format!("Bedrock stream failed: {error}")))?;
            decoder.push(&chunk);
            while let Some(frame) = decoder.next_message().map_err(Failure::new)? {
                let Some(item) = BedrockEvents::item(&frame) else { continue };
                if let Some(hook) = &options.on_provider_stream_event {
                    hook(&item, model);
                }
                self.events.frame(builder, &frame, &item)?;
            }
        }
        if decoder.has_partial() {
            return Err(Failure::new("Bedrock event stream ended in the middle of a message"));
        }
        self.events.result()
    }

    async fn send(
        &mut self,
        url: &reqwest::Url,
        headers: &[(String, String)],
        payload: &[u8],
        auth: &Auth,
        target: &Target,
        options: &StreamOptions,
    ) -> Result<reqwest::Response, Failure> {
        let client = client_for_env(&options.env);
        let mut sends = 0;
        loop {
            sends += 1;
            let mut request = client.post(url.clone()).body(payload.to_vec());
            let signed = match auth {
                Auth::Bearer(token) => vec![("authorization".to_string(), format!("Bearer {token}"))],
                Auth::Sign(credentials) => {
                    let signable = SignableRequest { method: "POST", url, headers, body: payload };
                    sign(&signable, credentials, &target.region, "bedrock", SystemTime::now())
                }
            };
            for (name, value) in headers.iter().chain(signed.iter()) {
                request = request.header(name.as_str(), value.as_str());
            }
            let failure = match request.send().await {
                Ok(response) => {
                    self.request_id = header(response.headers(), "x-amzn-requestid");
                    if response.status().is_success() {
                        return Ok(response);
                    }
                    let status = response.status().as_u16();
                    let response_headers = response.headers().clone();
                    let text = response.text().await.unwrap_or_default();
                    http_failure(status, &response_headers, &text)
                }
                Err(error) => Failure::new(format!("Request failed: {error}")),
            };
            let retryable = matches!(failure.status, Some(429 | 500 | 502 | 503 | 504) | None)
                || failure.error_code.as_deref().is_some_and(|code| code.contains("Throttling"));
            if sends >= MAX_SENDS || !retryable || options.cancel.is_cancelled() {
                return Err(failure);
            }
            tokio::time::sleep(Duration::from_millis(100 * 2u64.pow(sends - 1))).await;
        }
    }
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name).and_then(|value| value.to_str().ok()).map(str::to_string)
}

/// A non-2xx reply as the SDK reports it: `{prefix or code}: {message}`.
fn http_failure(status: u16, headers: &HeaderMap, body: &str) -> Failure {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let field = |keys: &[&str]| keys.iter().find_map(|key| parsed.as_ref()?.get(*key)?.as_str().map(str::to_string));
    let code = header(headers, "x-amzn-errortype")
        .or_else(|| field(&["__type", "code"]))
        .map(|code| {
            let code = code.rsplit('#').next().unwrap_or(&code);
            code.split(':').next().unwrap_or(code).to_string()
        })
        .filter(|code| !code.is_empty());
    let trimmed = body.trim();
    let message = match field(&["message", "Message", "errorMessage"]) {
        Some(message) => message,
        None if !trimmed.is_empty() => format!("{status}: {}", trimmed.chars().take(4000).collect::<String>()),
        None => "UnknownError".to_string(),
    };
    let name = code.as_deref().unwrap_or("Unknown");
    let prefix = error_prefix(name).unwrap_or(name);
    Failure {
        message: with_retention_hint(format!("{prefix}: {message}")),
        status: Some(status),
        error_code: code.filter(|code| code != "Unknown"),
    }
}
