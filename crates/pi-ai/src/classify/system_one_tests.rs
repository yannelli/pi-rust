//! Port of upstream `typesafe-system-one.test.ts`.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{Value, json};

use super::test_support::{answers_json, classifier_model, context, with_key};
use super::{ClassifierApi, ClassifierContext, ClassifierStopReason, TypeSafeSystemOne};
use crate::providers::mock_server::{MockResponse, MockServer};
use crate::request::RequestOptions;
use crate::types::Model;

fn model(base_url: &str) -> Model {
    classifier_model("typesafe-system-one", "typesafe", "jev-latest", base_url)
}

fn sample_context() -> ClassifierContext {
    context(json!({
        "state": { "text": "The deployment succeeded, thank you." },
        "questions": {
            "category": {
                "type": "choice",
                "instructions": "Classify the message",
                "criteria": { "success": "Successful", "failure": "Failed" },
            },
            "satisfaction": { "type": "score", "instructions": "Score satisfaction", "criteria": ["low", "neutral", "high"] },
            "approved": {
                "type": "bool",
                "instructions": "Does the user approve?",
                "criteria": { "true": "Approval", "false": "No approval" },
            },
        },
    }))
}

fn wire_answers() -> Value {
    json!({
        "category": { "type": "choice", "choice": "success", "probabilities": { "success": 0.9, "failure": 0.1 }, "confidence": 0.8 },
        "satisfaction": { "type": "score", "score": 2, "confidence": 0.7 },
        "approved": { "type": "noul", "noul": 0.95 },
    })
}

#[tokio::test]
async fn maps_public_bool_questions_and_answers_to_typesafe_noul_values() {
    let mut server = MockServer::sequence(vec![
        MockResponse::json(&json!({ "answers": wire_answers() })),
        MockResponse::json(
            &json!({ "answers": wire_answers(), "usage": { "input_tokens": 308, "output_tokens": 23 } }),
        ),
    ])
    .await;
    let model = model(&format!("{}/v1/", server.base_url));
    let options = super::ClassifierOptions { temperature: Some(1.5), ..with_key("secret") };
    let result = TypeSafeSystemOne.classify(&model, &sample_context(), options).await;
    let mut priced_model = model.clone();
    priced_model.cost.input = 0.042;
    let priced = TypeSafeSystemOne.classify(&priced_model, &sample_context(), with_key("secret")).await;

    let request = server.request().await;
    assert_eq!(request.path, "/v1/systemone");
    assert_eq!(request.body["model"], "jev-latest");
    let types: Vec<&Value> = request.body["questions"].as_object().unwrap().values().map(|q| &q["type"]).collect();
    assert_eq!(types, ["choice", "score", "noul"]);
    // System One has no temperature field; the option is ignored.
    assert!(request.body.get("temperature").is_none());
    assert_eq!(request.headers["authorization"], "Bearer secret");

    assert_eq!(result.stop_reason, ClassifierStopReason::Stop);
    let answers = answers_json(&result);
    assert_eq!(answers["approved"], json!({ "type": "bool", "probability": 0.95 }));
    assert_eq!(answers["category"]["choice"], "success");
    assert_eq!(answers["satisfaction"], json!({ "type": "score", "score": 2.0, "confidence": 0.7 }));
    assert_eq!(result.usage, None);
    let usage = priced.usage.expect("priced usage");
    assert_eq!((usage.input, usage.output, usage.total_tokens), (308, 23, 331));
    assert!((usage.cost.total - 0.000012936).abs() < 1e-12);
}

#[tokio::test]
async fn posts_openrouter_system_one_requests_to_its_typesafe_compatible_endpoint() {
    let response = json!({
        "id": "gen-dec-1",
        "provider": "TypeSafe",
        "answers": wire_answers(),
        "usage": { "input_tokens": 308, "output_tokens": 23, "cost": 0.000012936 },
    });
    let mut server = MockServer::start(MockResponse::json(&response)).await;
    let mut model = classifier_model(
        "typesafe-system-one",
        "openrouter",
        "typesafe/jev-1.13",
        &format!("{}/api/v1", server.base_url),
    );
    model.cost.input = 0.042;
    let context = sample_context();
    let result = TypeSafeSystemOne.classify(&model, &context, with_key("secret")).await;

    let request = server.request().await;
    assert_eq!(request.path, "/api/v1/systemone");
    assert_eq!(request.body["model"], "typesafe/jev-1.13");
    assert_eq!(request.body["state"], Value::Object(context.state.clone()));
    assert_eq!(result.stop_reason, ClassifierStopReason::Stop);
    assert_eq!(answers_json(&result)["approved"], json!({ "type": "bool", "probability": 0.95 }));
    // Priced from the catalog like chat usage; matches OpenRouter's reported cost.
    assert!((result.usage.unwrap().cost.total - 0.000012936).abs() < 1e-12);
}

#[tokio::test]
async fn rejects_models_for_other_classifier_apis_and_image_input_before_sending() {
    let server = MockServer::sequence(Vec::new()).await;
    let mut other = model(&server.base_url);
    other.api = "cloudflare-workers-ai-system-one".into();
    let result = TypeSafeSystemOne.classify(&other, &sample_context(), with_key("secret")).await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Error);
    assert!(result.error_message.unwrap().contains("Unsupported classifier API: cloudflare-workers-ai-system-one"));

    let mut context = sample_context();
    context.images = Some(vec![super::ImageContent { data: "aW1hZ2U=".into(), mime_type: "image/png".into() }]);
    let result = TypeSafeSystemOne.classify(&model(&server.base_url), &context, with_key("secret")).await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Error);
    assert_eq!(result.error_message.as_deref(), Some("System One API does not support image input for jev-latest"));
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn merges_headers_case_insensitively_and_supports_suppression() {
    let answers = MockResponse::json(&json!({ "answers": wire_answers() }));
    let mut server = MockServer::sequence(vec![answers.clone(), answers]).await;
    let mut model = model(&server.base_url);
    model.headers = BTreeMap::from([
        ("authorization".to_string(), "Bearer model".to_string()),
        ("X-Source".to_string(), "model".to_string()),
    ]);
    let headers = BTreeMap::from([
        ("Authorization".to_string(), "Bearer request".to_string()),
        ("x-source".to_string(), "request".to_string()),
    ]);
    let mut options = with_key("secret");
    options.request.headers = headers;
    TypeSafeSystemOne.classify(&model, &sample_context(), options).await;
    let mut options = with_key("secret");
    options.request.suppressed_headers = vec!["Authorization".into()];
    TypeSafeSystemOne.classify(&model, &sample_context(), options).await;

    let first = server.request().await;
    assert_eq!(
        (first.headers["authorization"].as_str(), first.headers["x-source"].as_str()),
        ("Bearer request", "request")
    );
    assert!(!server.request().await.headers.contains_key("authorization"));
}

#[tokio::test]
async fn preserves_prototype_sensitive_question_ids_in_answers() {
    let response = json!({ "answers": { "__proto__": { "type": "noul", "noul": 0.75 } } });
    let server = MockServer::start(MockResponse::json(&response)).await;
    let context = context(json!({
        "state": {},
        "questions": { "__proto__": { "type": "bool", "instructions": "Is this true?", "criteria": { "true": "Yes", "false": "No" } } },
    }));
    let result = TypeSafeSystemOne.classify(&model(&server.base_url), &context, with_key("secret")).await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Stop);
    assert_eq!(result.answers.keys().collect::<Vec<_>>(), ["__proto__"]);
    assert_eq!(answers_json(&result), json!({ "__proto__": { "type": "bool", "probability": 0.75 } }));
}

#[tokio::test]
async fn reports_request_timeouts_separately_from_caller_cancellation() {
    let slow = MockResponse::Delayed(Duration::from_secs(5), Box::new(MockResponse::json(&json!({}))));
    let server = MockServer::start(slow).await;
    let mut options = with_key("secret");
    options.request = RequestOptions { timeout_ms: Some(5), max_retries: Some(0), ..options.request };
    let result = TypeSafeSystemOne.classify(&model(&server.base_url), &sample_context(), options).await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Error);
    assert_eq!(result.error_message.as_deref(), Some("Request timed out after 5ms"));
}

#[tokio::test]
async fn retries_with_a_fresh_timeout_for_every_request() {
    let server = MockServer::sequence(vec![
        MockResponse::with_headers(500, &[("retry-after-ms", "0")], "retry"),
        MockResponse::json(&json!({ "answers": wire_answers() })),
    ])
    .await;
    let mut options = with_key("secret");
    options.request = RequestOptions { timeout_ms: Some(1000), max_retries: Some(1), ..options.request };
    let result = TypeSafeSystemOne.classify(&model(&server.base_url), &sample_context(), options).await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Stop);
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn returns_malformed_responses_and_http_failures_as_classifier_errors() {
    let malformed = json!({ "answers": {}, "usage": { "input_tokens": 10, "output_tokens": 2 } });
    let server =
        MockServer::sequence(vec![MockResponse::json(&malformed), MockResponse::Status(401, " bad key ".into())]).await;
    let model = model(&server.base_url);
    let result = TypeSafeSystemOne.classify(&model, &sample_context(), with_key("secret")).await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Error);
    assert!(result.answers.is_empty());
    assert!(result.error_message.unwrap().contains("did not return an answer for category"));
    // The request was billed, so its usage is kept.
    let usage = result.usage.expect("billed usage");
    assert_eq!((usage.input, usage.output), (10, 2));

    let result = TypeSafeSystemOne.classify(&model, &sample_context(), with_key("secret")).await;
    assert_eq!(result.error_message.as_deref(), Some("System One API error (401): bad key"));
}

#[tokio::test]
async fn ignores_malformed_usage() {
    let server = MockServer::sequence(vec![
        MockResponse::json(
            &json!({ "answers": wire_answers(), "usage": { "input_tokens": "many", "output_tokens": 3 } }),
        ),
        MockResponse::json(&json!({ "answers": wire_answers(), "usage": { "cost": 0.1 } })),
    ])
    .await;
    let model = model(&server.base_url);
    let result = TypeSafeSystemOne.classify(&model, &sample_context(), with_key("secret")).await;
    let without_tokens = TypeSafeSystemOne.classify(&model, &sample_context(), with_key("secret")).await;

    assert_eq!(result.stop_reason, ClassifierStopReason::Stop);
    let usage = result.usage.expect("usage");
    assert_eq!((usage.input, usage.output, usage.total_tokens), (0, 3, 3));
    assert_eq!(without_tokens.stop_reason, ClassifierStopReason::Stop);
    assert_eq!(without_tokens.usage, None);
}
