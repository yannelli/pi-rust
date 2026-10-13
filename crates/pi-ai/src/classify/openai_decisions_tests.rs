//! Port of upstream `openai-decisions.test.ts`.

use serde_json::{Value, json};

use super::test_support::{answers_json, classifier_model, context, with_key};
use super::{ClassifierApi, ClassifierContext, ClassifierOptions, ClassifierStopReason, ImageContent, OpenAIDecisions};
use crate::providers::mock_server::{MockResponse, MockServer};
use crate::types::{Model, ModelCost, ModelCostTier};

fn model(server: &MockServer) -> Model {
    let mut model = classifier_model("openai-decisions", "openai", "gpt-6-luna", &format!("{}/v1", server.base_url));
    model.input = vec!["text".into(), "image".into()];
    model.context_window = 922_000;
    let tier = ModelCostTier { input_tokens_above: 272_000, input: 0.2, ..ModelCostTier::default() };
    model.cost = ModelCost { input: 0.1, tiers: vec![tier], ..ModelCost::default() };
    model
}

fn sample_context() -> ClassifierContext {
    context(json!({
        "state": { "text": "The deployment succeeded, thank you." },
        "questions": {
            "category": { "type": "choice", "instructions": "Classify the message", "criteria": { "success": "Successful", "failure": "" } },
            "satisfaction": { "type": "score", "instructions": "Score satisfaction", "criteria": ["low", "neutral", "high"] },
            "approved": { "type": "bool", "instructions": "Does the user approve?", "criteria": { "true": "Approval", "false": "No approval" } },
        },
    }))
}

/// Response shape from the API reference and live `gpt-6-luna` requests.
fn wire_answers() -> Vec<Value> {
    vec![
        json!({
            "type": "choice",
            "name": "category",
            "choice": "success",
            "probabilities": [{ "value": "success", "probability": 0.9 }, { "value": "failure", "probability": 0.1 }],
            "confidence": 0.8,
        }),
        json!({
            "type": "score",
            "name": "satisfaction",
            "score": 1.8,
            "probabilities": [
                { "value": 0, "label": "low", "probability": 0.05 },
                { "value": 1, "label": "neutral", "probability": 0.1 },
                { "value": 2, "label": "high", "probability": 0.85 },
            ],
            "confidence": 0.7,
        }),
        json!({ "type": "predicate", "name": "approved", "probability": 0.95 }),
    ]
}

fn wire_usage() -> Value {
    json!({
        "input_tokens": 164,
        "input_tokens_details": { "cached_tokens": 0, "cache_write_tokens": 0 },
        "output_tokens": 0,
        "output_tokens_details": { "reasoning_tokens": 0 },
        "total_tokens": 164,
    })
}

fn image() -> ImageContent {
    ImageContent { data: "aW1hZ2U=".into(), mime_type: "image/png".into() }
}

async fn run(server: &MockServer, context: &ClassifierContext, options: ClassifierOptions) -> super::ClassifierResult {
    OpenAIDecisions.classify(&model(server), context, options).await
}

#[tokio::test]
async fn maps_questions_to_decisions_types_and_answers_back_by_name() {
    // Answers out of question order: they are matched by name.
    let reversed: Vec<Value> = wire_answers().into_iter().rev().collect();
    let response = json!({ "model": "gpt-6-luna", "answers": reversed, "usage": wire_usage() });
    let mut server = MockServer::start(MockResponse::json(&response)).await;
    let context = sample_context();
    let result = run(&server, &context, ClassifierOptions { temperature: Some(1.5), ..with_key("secret") }).await;

    let request = server.request().await;
    assert_eq!(request.path, "/v1/decisions");
    assert_eq!(request.headers["authorization"], "Bearer secret");
    let expected = json!({
        "model": "gpt-6-luna",
        "input": Value::Object(context.state.clone()).to_string(),
        "questions": [
            {
                "type": "choice",
                "name": "category",
                "instructions": "Classify the message",
                // Empty descriptions are left out.
                "choices": [{ "value": "success", "description": "Successful" }, { "value": "failure" }],
            },
            {
                "type": "score",
                "name": "satisfaction",
                "instructions": "Score satisfaction",
                "levels": [{ "label": "low" }, { "label": "neutral" }, { "label": "high" }],
            },
            {
                "type": "predicate",
                "name": "approved",
                "instructions": "Does the user approve?\n\nTrue means: Approval\nFalse means: No approval",
            },
        ],
    });
    assert_eq!(request.body, expected);
    assert_eq!(result.stop_reason, ClassifierStopReason::Stop);
    let answers = json!({
        "category": { "type": "choice", "choice": "success", "probabilities": { "success": 0.9, "failure": 0.1 }, "confidence": 0.8 },
        "satisfaction": { "type": "score", "score": 1.8, "confidence": 0.7 },
        "approved": { "type": "bool", "probability": 0.95 },
    });
    assert_eq!(answers_json(&result), answers);
    let usage = result.usage.expect("usage");
    assert_eq!((usage.input, usage.output, usage.cache_read, usage.total_tokens), (164, 0, 0, 164));
    assert!((usage.cost.total - 0.0000164).abs() < 1e-12);
}

#[tokio::test]
async fn prices_long_context_requests_at_the_long_context_input_rate() {
    let response = json!({ "answers": wire_answers(), "usage": { "input_tokens": 300000, "output_tokens": 0 } });
    let server = MockServer::start(MockResponse::json(&response)).await;
    let result = run(&server, &sample_context(), with_key("secret")).await;
    assert!((result.usage.unwrap().cost.total - 0.06).abs() < 1e-12);
}

#[tokio::test]
async fn sends_images_after_the_state_in_one_user_message() {
    let mut server = MockServer::start(MockResponse::json(&json!({ "answers": wire_answers() }))).await;
    let mut context = sample_context();
    context.images = Some(vec![image(), ImageContent { mime_type: "image/jpeg".into(), ..image() }]);
    let result = run(&server, &context, with_key("secret")).await;

    assert_eq!(result.stop_reason, ClassifierStopReason::Stop);
    let input = json!([{
        "role": "user",
        "content": [
            { "type": "input_text", "text": Value::Object(context.state.clone()).to_string() },
            { "type": "input_image", "image_url": "data:image/png;base64,aW1hZ2U=" },
            { "type": "input_image", "image_url": "data:image/jpeg;base64,aW1hZ2U=" },
        ],
    }]);
    assert_eq!(server.request().await.body["input"], input);
}

#[tokio::test]
async fn rejects_more_than_128_images_before_sending() {
    let server = MockServer::sequence(Vec::new()).await;
    let mut context = sample_context();
    context.images = Some(vec![image(); 129]);
    let result = run(&server, &context, with_key("secret")).await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Error);
    assert!(result.error_message.unwrap().contains("at most 128 images, got 129"));
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn fails_the_result_when_a_question_is_refused_and_keeps_the_billed_usage() {
    let mut answers = wire_answers();
    answers[2] = json!({ "type": "refusal", "name": "approved" });
    let server = MockServer::start(MockResponse::json(&json!({ "answers": answers, "usage": wire_usage() }))).await;
    let result = run(&server, &sample_context(), with_key("secret")).await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Error);
    assert!(result.answers.is_empty());
    assert_eq!(result.error_message.as_deref(), Some("OpenAI Decisions refused to answer approved"));
    assert_eq!(result.usage.unwrap().input, 164);
}

#[tokio::test]
async fn returns_missing_and_mistyped_answers_as_classifier_errors() {
    let mut mistyped = wire_answers();
    mistyped[2] = json!({ "type": "score", "name": "approved" });
    let server = MockServer::sequence(vec![
        MockResponse::json(&json!({ "answers": wire_answers()[..2] })),
        MockResponse::json(&json!({ "answers": mistyped })),
    ])
    .await;
    let missing = run(&server, &sample_context(), with_key("secret")).await;
    let mistyped = run(&server, &sample_context(), with_key("secret")).await;
    assert_eq!(missing.stop_reason, ClassifierStopReason::Error);
    assert!(missing.error_message.unwrap().contains("did not return an answer for approved"));
    assert_eq!(mistyped.stop_reason, ClassifierStopReason::Error);
    assert!(mistyped.error_message.unwrap().contains("did not return a predicate answer for approved"));
}

#[tokio::test]
async fn preserves_prototype_sensitive_question_ids_in_answers() {
    let response = json!({ "answers": [{ "type": "predicate", "name": "__proto__", "probability": 0.75 }] });
    let server = MockServer::start(MockResponse::json(&response)).await;
    let context = context(json!({
        "state": {},
        "questions": { "__proto__": { "type": "bool", "instructions": "Is this true?", "criteria": { "true": "Yes", "false": "No" } } },
    }));
    let result = run(&server, &context, with_key("secret")).await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Stop);
    assert_eq!(answers_json(&result), json!({ "__proto__": { "type": "bool", "probability": 0.75 } }));
}

#[tokio::test]
async fn does_not_retry_gateway_timeouts_and_explains_them_instead_of_returning_the_html_page() {
    let page = "<!DOCTYPE html><html>Gateway time-out</html>";
    let server = MockServer::sequence(vec![
        MockResponse::with_headers(504, &[("retry-after-ms", "0"), ("content-type", "text/html")], page),
        MockResponse::json(&json!({ "answers": wire_answers() })),
    ])
    .await;
    // Default retries: the same input would time out again.
    let result = run(&server, &sample_context(), with_key("secret")).await;
    assert_eq!(server.requests().len(), 1);
    assert_eq!(result.stop_reason, ClassifierStopReason::Error);
    let message = result.error_message.unwrap();
    assert!(message.contains("OpenAI Decisions error (504): the request timed out at the gateway"), "{message}");
    assert!(!message.contains("<html>"));
}

#[tokio::test]
async fn still_retries_other_server_errors() {
    let server = MockServer::sequence(vec![
        MockResponse::with_headers(503, &[("retry-after-ms", "0")], "busy"),
        MockResponse::json(&json!({ "answers": wire_answers() })),
    ])
    .await;
    let result = run(&server, &sample_context(), with_key("secret")).await;
    assert_eq!(server.requests().len(), 2);
    assert_eq!(result.stop_reason, ClassifierStopReason::Stop);
}

#[tokio::test]
async fn includes_the_api_error_body_for_other_http_failures() {
    let body =
        json!({ "error": { "message": "Decision input exceeds the token limit.", "type": "invalid_request_error" } });
    let server = MockServer::start(MockResponse::Status(400, body.to_string())).await;
    let mut options = with_key("secret");
    options.request.max_retries = Some(0);
    let result = run(&server, &sample_context(), options).await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Error);
    let message = result.error_message.unwrap();
    assert!(message.contains("OpenAI Decisions error (400)"), "{message}");
    assert!(message.contains("Decision input exceeds the token limit."));
}

#[tokio::test]
async fn rejects_models_for_other_classifier_apis_and_missing_api_keys() {
    let server = MockServer::sequence(Vec::new()).await;
    let mut other = model(&server);
    other.api = "typesafe-system-one".into();
    let other_api = OpenAIDecisions.classify(&other, &sample_context(), with_key("secret")).await;
    let no_key = run(&server, &sample_context(), ClassifierOptions::default()).await;

    assert!(server.requests().is_empty());
    assert!(other_api.error_message.unwrap().contains("Unsupported classifier API: typesafe-system-one"));
    assert!(no_key.error_message.unwrap().contains("No API key for provider: openai"));
}
