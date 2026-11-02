//! Request-level port of upstream `classifier-models.test.ts`: the `classify` entry point,
//! its registry, and the model checks it runs before calling an API.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::future::BoxFuture;
use serde_json::{Value, json};

use super::test_support::{answers_json, classifier_model, context, with_key};
use super::{
    ClassifierAnswer, ClassifierApi, ClassifierContext, ClassifierOptions, ClassifierResult, ClassifierStopReason,
    ImageContent, classifier_api, classify, register_classifier_api,
};
use crate::providers::mock_server::{MockResponse, MockServer};
use crate::types::Model;

fn approval_context() -> ClassifierContext {
    context(json!({
        "state": { "text": "yes" },
        "questions": {
            "approved": { "type": "bool", "instructions": "Does this express approval?", "criteria": { "true": "Approval", "false": "No approval" } },
        },
    }))
}

fn image() -> ImageContent {
    ImageContent { data: "aW1hZ2U=".into(), mime_type: "image/png".into() }
}

/// A test API that answers `approved` and counts its calls.
#[derive(Default)]
struct Approves(AtomicUsize);

impl ClassifierApi for Approves {
    fn classify(
        &self,
        model: &Model,
        _: &ClassifierContext,
        _: ClassifierOptions,
    ) -> BoxFuture<'static, ClassifierResult> {
        self.0.fetch_add(1, Ordering::Relaxed);
        let mut result = ClassifierResult::new(model);
        result.answers.insert("approved", ClassifierAnswer::Bool { probability: 0.9 });
        Box::pin(async move { result })
    }
}

#[test]
fn registers_the_built_in_classifier_apis() {
    for api in ["typesafe-system-one", "cloudflare-workers-ai-system-one", "openai-decisions", "llama-cpp-classify"] {
        assert!(classifier_api(api).is_some(), "{api}");
    }
}

#[test]
fn entry_point_futures_can_move_across_threads() {
    fn assert_send<T: Send>(_: T) {}
    let model = classifier_model("typesafe-system-one", "test", "m", "https://example.test/v1");
    assert_send(classify(&model, &approval_context(), ClassifierOptions::default()));
    let images = crate::images::ImagesContext::default();
    assert_send(crate::images::generate_images(&model, &images, crate::images::ImagesOptions::default()));
}

#[tokio::test]
async fn dispatches_to_the_api_registered_for_the_model() {
    register_classifier_api("test-classifier-dispatch", Arc::new(Approves::default()));
    let model = classifier_model("test-classifier-dispatch", "test", "shared", "https://example.test/v1");
    let result = classify(&model, &approval_context(), ClassifierOptions::default()).await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Stop);
    assert_eq!(answers_json(&result)["approved"], json!({ "type": "bool", "probability": 0.9 }));

    let missing = classifier_model("test-classifier-missing", "test", "m", "https://example.test/v1");
    let result = classify(&missing, &approval_context(), ClassifierOptions::default()).await;
    assert_eq!(result.error_message.as_deref(), Some("No API provider registered for api: test-classifier-missing"));
}

#[tokio::test]
async fn rejects_chat_models_at_the_classifier_entry_point() {
    let mut chat = classifier_model("typesafe-system-one", "test", "chat", "https://example.test/v1");
    chat.model_type = None;
    let result = classify(&chat, &approval_context(), with_key("secret")).await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Error);
    assert_eq!(result.error_message.as_deref(), Some("Model test/chat is not a classifier model"));
    assert!(result.answers.is_empty());

    let options = with_key("secret");
    options.request.cancel.cancel();
    assert_eq!(classify(&chat, &approval_context(), options).await.stop_reason, ClassifierStopReason::Aborted);
}

#[tokio::test]
async fn routes_jev_on_gateways_to_their_typesafe_compatible_endpoints() {
    let models = crate::models::builtin_models();
    for (provider, id, expected_base) in [
        ("vercel-ai-gateway", "typesafe-ai/jev", "https://ai-gateway.vercel.sh/typesafe/v1"),
        ("opencode", "jev-1.13", "https://opencode.ai/inference/systemone/v1"),
        ("opencode", "jev-1.13-free", "https://opencode.ai/inference/systemone/v1"),
    ] {
        let mut jev = models
            .iter()
            .find(|model| {
                model.provider == provider && model.id == id && model.model_type.as_deref() == Some("classifier")
            })
            .cloned()
            .expect("the bundled catalog contains the classifier");
        assert_eq!((jev.api.as_str(), jev.context_window), ("typesafe-system-one", 32_000));
        assert_eq!(jev.base_url, expected_base);
        assert!(!models.iter().any(|model| {
            model.provider == provider && model.id == id && matches!(model.model_type.as_deref(), None | Some("chat"))
        }));
        let prefix = reqwest::Url::parse(&jev.base_url).unwrap().path().to_owned();
        let response = json!({ "model": id, "answers": { "approved": { "type": "noul", "noul": 0.8 } } });
        let mut server = MockServer::start(MockResponse::json(&response)).await;
        jev.base_url = format!("{}{prefix}", server.base_url);
        let context = approval_context();
        let result = classify(&jev, &context, with_key("secret")).await;

        let request = server.request().await;
        assert_eq!(request.path, format!("{prefix}/systemone"));
        assert_eq!((request.body["model"].as_str(), &request.body["state"]), (Some(id), &Value::Object(context.state)));
        assert_eq!(request.headers["authorization"], "Bearer secret");
        assert_eq!(result.stop_reason, ClassifierStopReason::Stop);
        assert_eq!(answers_json(&result)["approved"], json!({ "type": "bool", "probability": 0.8 }));
    }
}

#[tokio::test]
async fn rejects_images_for_models_without_image_input_before_calling_the_api() {
    let api = Arc::new(Approves::default());
    register_classifier_api("test-classifier-images", api.clone());
    let text_only = classifier_model("test-classifier-images", "test", "text-only", "https://example.test/v1");
    let mut with_images = approval_context();
    with_images.images = Some(vec![image()]);
    let result = classify(&text_only, &with_images, ClassifierOptions::default()).await;
    with_images.images = Some(Vec::new());
    let without_images = classify(&text_only, &with_images, ClassifierOptions::default()).await;

    assert_eq!(result.stop_reason, ClassifierStopReason::Error);
    assert_eq!(result.error_message.as_deref(), Some("Model test/text-only does not accept image input"));
    assert_eq!(without_images.stop_reason, ClassifierStopReason::Stop);
    assert_eq!(api.0.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn routes_openai_decisions_models_with_images() {
    let response = json!({ "answers": [{ "type": "predicate", "name": "approved", "probability": 0.8 }] });
    let mut server = MockServer::start(MockResponse::json(&response)).await;
    let mut luna = classifier_model("openai-decisions", "openai", "gpt-6-luna", &format!("{}/v1", server.base_url));
    luna.input = vec!["text".into(), "image".into()];
    let mut context = approval_context();
    context.images = Some(vec![image()]);
    let result = classify(&luna, &context, with_key("secret")).await;

    assert_eq!(server.request().await.path, "/v1/decisions");
    assert_eq!(result.stop_reason, ClassifierStopReason::Stop);
    assert_eq!(answers_json(&result)["approved"], json!({ "type": "bool", "probability": 0.8 }));
}

#[test]
fn context_and_result_round_trip_with_upstream_field_names() {
    let json = json!({
        "state": { "b": 1, "a": 2 },
        "images": [{ "type": "image", "data": "aW1hZ2U=", "mimeType": "image/png" }],
        "questions": {
            "z": { "type": "score", "instructions": "Rate", "criteria": ["low", "high"] },
            "a": { "type": "choice", "instructions": "Pick", "criteria": { "y": "Yes", "x": "" } },
        },
    });
    let parsed = context(json.clone());
    assert_eq!(serde_json::to_value(&parsed).unwrap(), json);
    let mut result = ClassifierResult::new(&classifier_model("api", "p", "m", ""));
    result.fail("boom", false);
    let value = serde_json::to_value(&result).unwrap();
    assert_eq!((value["stopReason"].as_str(), value["errorMessage"].as_str()), (Some("error"), Some("boom")));
    assert!(value.get("usage").is_none());
}
