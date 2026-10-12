//! TypeSafe System One on its native endpoint and on Cloudflare Workers AI (upstream
//! `system-one-shared.ts`, `typesafe-system-one.ts`, `cloudflare-workers-ai-system-one.ts`).

use std::collections::BTreeMap;

use futures::future::BoxFuture;
use serde_json::{Value, json};

use super::shared::{
    endpoint, error_message, failure, js_string, parse_classifier_usage, post_classifier_request, required_number,
};
use super::{
    ClassifierAnswer, ClassifierApi, ClassifierContext, ClassifierOptions, ClassifierQuestion, ClassifierResult,
    OrderedMap,
};
use crate::retry::ProviderRequestError;
use crate::types::{JsonObject, Model};

/// Differences between services that serve System One models.
struct Transport {
    api: &'static str,
    /// Service name used in error messages.
    label: &'static str,
    url: fn(&Model, &BTreeMap<String, String>) -> String,
    /// Wraps the System One request in the service's request envelope.
    payload: fn(&Model, JsonObject) -> Value,
    /// Extracts the System One output (`{ answers, usage }`) from the response envelope.
    output: fn(Value) -> Result<JsonObject, ProviderRequestError>,
}

/// TypeSafe's native protocol. OpenRouter, Vercel AI Gateway, and OpenCode serve it too.
const TYPESAFE: Transport = Transport {
    api: "typesafe-system-one",
    label: "System One API",
    url: |model, _| endpoint(&model.base_url, "systemone"),
    payload: |model, request| {
        let mut payload = JsonObject::new();
        payload.insert("model".into(), model.id.clone().into());
        payload.extend(request);
        Value::Object(payload)
    },
    output: |body| match body {
        Value::Object(body) => Ok(body),
        _ => Err(failure("System One API returned an unexpected response")),
    },
};

const CLOUDFLARE: &str = "Cloudflare Workers AI";

/// Workers AI REST endpoint: `POST /accounts/{account}/ai/run` with `{ model, input }`.
const CLOUDFLARE_WORKERS_AI: Transport = Transport {
    api: "cloudflare-workers-ai-system-one",
    label: CLOUDFLARE,
    url: |model, env| endpoint(&resolve_cloudflare_base_url(&model.base_url, env), "run"),
    payload: |model, request| json!({ "model": model.id, "input": request }),
    output: cloudflare_output,
};

/// Fill `{CLOUDFLARE_ACCOUNT_ID}` and `{CLOUDFLARE_GATEWAY_ID}` from the provider env, then the process env.
pub(crate) fn resolve_cloudflare_base_url(base_url: &str, env: &BTreeMap<String, String>) -> String {
    let mut url = base_url.to_string();
    for name in ["CLOUDFLARE_ACCOUNT_ID", "CLOUDFLARE_GATEWAY_ID"] {
        let placeholder = format!("{{{name}}}");
        if !url.contains(&placeholder) {
            continue;
        }
        if let Some(value) = env.get(name).cloned().or_else(|| std::env::var(name).ok()) {
            url = url.replace(&placeholder, &value);
        }
    }
    url
}

fn cloudflare_error_message(errors: Option<&Value>) -> String {
    let messages: Vec<&str> = errors
        .and_then(Value::as_array)
        .map(|errors| errors.iter().filter_map(|error| error.get("message")?.as_str()).collect())
        .unwrap_or_default();
    if messages.is_empty() {
        return format!("{CLOUDFLARE} request failed");
    }
    format!("{CLOUDFLARE} error: {}", messages.join("; "))
}

/// Third-party models such as `typesafe/jev` wrap the output in a run record
/// (`{ success, result: { state, result } }`); Cloudflare-hosted models return it directly.
fn cloudflare_output(body: Value) -> Result<JsonObject, ProviderRequestError> {
    let unexpected = || failure(format!("{CLOUDFLARE} returned an unexpected response"));
    let Value::Object(mut body) = body else { return Err(unexpected()) };
    if body.get("success") == Some(&Value::Bool(false)) {
        return Err(failure(cloudflare_error_message(body.get("errors"))));
    }
    let Some(Value::Object(mut result)) = body.remove("result") else { return Err(unexpected()) };
    if result.contains_key("answers") {
        return Ok(result);
    }
    if result.get("state").and_then(Value::as_str) != Some("Completed") {
        let state = js_string(result.get("state"));
        return Err(failure(format!("{CLOUDFLARE} run did not complete (state: {state})")));
    }
    match result.remove("result") {
        Some(Value::Object(output)) => Ok(output),
        _ => Err(unexpected()),
    }
}

/// System One classification on TypeSafe-compatible endpoints (`POST {baseUrl}/systemone`).
#[derive(Clone, Copy, Debug, Default)]
pub struct TypeSafeSystemOne;

/// System One models on the Cloudflare Workers AI REST endpoint (`POST {baseUrl}/run`).
#[derive(Clone, Copy, Debug, Default)]
pub struct CloudflareWorkersAISystemOne;

impl ClassifierApi for TypeSafeSystemOne {
    fn classify(
        &self,
        model: &Model,
        context: &ClassifierContext,
        options: ClassifierOptions,
    ) -> BoxFuture<'static, ClassifierResult> {
        let (model, context) = (model.clone(), context.clone());
        Box::pin(async move { classify_system_one(&TYPESAFE, &model, &context, &options).await })
    }
}

impl ClassifierApi for CloudflareWorkersAISystemOne {
    fn classify(
        &self,
        model: &Model,
        context: &ClassifierContext,
        options: ClassifierOptions,
    ) -> BoxFuture<'static, ClassifierResult> {
        let (model, context) = (model.clone(), context.clone());
        Box::pin(async move { classify_system_one(&CLOUDFLARE_WORKERS_AI, &model, &context, &options).await })
    }
}

async fn classify_system_one(
    transport: &Transport,
    model: &Model,
    context: &ClassifierContext,
    options: &ClassifierOptions,
) -> ClassifierResult {
    let mut output = ClassifierResult::new(model);
    if let Err(error) = run(transport, model, context, options, &mut output).await {
        let message = error_message(&error, Some(&format!("{} error", transport.label)));
        output.fail(message, options.request.cancel.is_cancelled());
    }
    output
}

async fn run(
    transport: &Transport,
    model: &Model,
    context: &ClassifierContext,
    options: &ClassifierOptions,
    output: &mut ClassifierResult,
) -> Result<(), ProviderRequestError> {
    if model.api != transport.api {
        return Err(failure(format!("Unsupported classifier API: {}", model.api)));
    }
    if context.has_images() {
        return Err(failure(format!("{} does not support image input", transport.label)));
    }
    let request = &options.request;
    let url = (transport.url)(model, &request.env);
    let payload = (transport.payload)(model, wire_request(context));
    let body = post_classifier_request(transport.label, url, model, payload, request, &[]).await?;
    let result = (transport.output)(body)?;
    // Set before parsing answers: a request with malformed answers was still billed.
    output.usage = parse_classifier_usage(result.get("usage"), model);
    output.answers = parse_answers(transport.label, result.get("answers"), context)?;
    Ok(())
}

/// The System One request; public `bool` questions use TypeSafe's wire-level `noul` type.
fn wire_request(context: &ClassifierContext) -> JsonObject {
    let questions: JsonObject = context
        .questions
        .iter()
        .map(|(id, question)| {
            let mut value = serde_json::to_value(question).unwrap_or_default();
            if matches!(question, ClassifierQuestion::Bool { .. }) {
                value["type"] = "noul".into();
            }
            (id.to_string(), value)
        })
        .collect();
    let mut request = JsonObject::new();
    request.insert("state".into(), Value::Object(context.state.clone()));
    request.insert("questions".into(), Value::Object(questions));
    request
}

fn probabilities(label: &str, value: Option<&Value>, id: &str) -> Result<OrderedMap<f64>, ProviderRequestError> {
    let Some(entries) = value.and_then(Value::as_object) else {
        return Err(failure(format!("{label} returned invalid probabilities for {id}")));
    };
    entries
        .iter()
        .map(|(key, probability)| {
            Ok((key.as_str(), required_number(label, Some(probability), &format!("probability for {id}.{key}"))?))
        })
        .collect()
}

fn parse_answer(
    label: &str,
    id: &str,
    question: &ClassifierQuestion,
    answer: &JsonObject,
) -> Result<ClassifierAnswer, ProviderRequestError> {
    let kind = answer.get("type").and_then(Value::as_str);
    let confidence = || required_number(label, answer.get("confidence"), &format!("confidence for {id}"));
    match question {
        ClassifierQuestion::Choice { .. } => {
            let choice = answer.get("choice").and_then(Value::as_str).filter(|_| kind == Some("choice"));
            let Some(choice) = choice else {
                return Err(failure(format!("{label} did not return a choice answer for {id}")));
            };
            Ok(ClassifierAnswer::Choice {
                choice: choice.to_string(),
                probabilities: probabilities(label, answer.get("probabilities"), id)?,
                confidence: confidence()?,
            })
        }
        ClassifierQuestion::Score { .. } => {
            if kind != Some("score") {
                return Err(failure(format!("{label} did not return a score answer for {id}")));
            }
            let score = required_number(label, answer.get("score"), &format!("score for {id}"))?;
            Ok(ClassifierAnswer::Score { score, confidence: confidence()? })
        }
        ClassifierQuestion::Bool { .. } => {
            if kind != Some("noul") {
                return Err(failure(format!("{label} did not return a bool answer for {id}")));
            }
            let probability = required_number(label, answer.get("noul"), &format!("probability for {id}"))?;
            Ok(ClassifierAnswer::Bool { probability })
        }
    }
}

fn parse_answers(
    label: &str,
    value: Option<&Value>,
    context: &ClassifierContext,
) -> Result<OrderedMap<ClassifierAnswer>, ProviderRequestError> {
    let Some(answers) = value.and_then(Value::as_object) else {
        return Err(failure(format!("{label} returned an unexpected response")));
    };
    let mut parsed = OrderedMap::new();
    for (id, question) in context.questions.iter() {
        let Some(answer) = answers.get(id).and_then(Value::as_object) else {
            return Err(failure(format!("{label} did not return an answer for {id}")));
        };
        parsed.insert(id, parse_answer(label, id, question, answer)?);
    }
    Ok(parsed)
}
