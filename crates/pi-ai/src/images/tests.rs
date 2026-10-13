//! Ports of upstream `openrouter-images.test.ts` and `images.test.ts` against a local server.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::{AssistantImages, ImagesContext, ImagesOptions, ImagesStopReason, generate_images, images_api};
use crate::providers::mock_server::{MockResponse, MockServer};
use crate::request::RequestOptions;
use crate::types::{ContentBlock, Model, ModelCost};

fn model(server: &MockServer, id: &str, output: &[&str]) -> Model {
    Model {
        id: id.into(),
        name: id.into(),
        api: "openrouter-images".into(),
        provider: "openrouter".into(),
        base_url: format!("{}/api/v1", server.base_url),
        model_type: Some("image".into()),
        input: vec!["text".into(), "image".into()],
        output: Some(output.iter().map(|modality| modality.to_string()).collect()),
        cost: ModelCost { input: 0.015, output: 0.03, ..ModelCost::default() },
        ..Model::default()
    }
}

fn prompt(text: &str) -> ImagesContext {
    ImagesContext { input: vec![ContentBlock::text(text)] }
}

fn with_key(api_key: &str) -> ImagesOptions {
    ImagesOptions {
        request: RequestOptions { api_key: Some(api_key.into()), ..RequestOptions::default() },
        metadata: None,
    }
}

fn image_response() -> Value {
    json!({
        "id": "img-1",
        "usage": { "prompt_tokens": 12, "completion_tokens": 34, "prompt_tokens_details": { "cached_tokens": 0 } },
        "choices": [{
            "message": {
                "content": "Here is your image.",
                "images": [{ "image_url": "data:image/png;base64,ZmFrZS1wbmc=" }],
            },
        }],
    })
}

fn has_image(output: &AssistantImages) -> bool {
    output.output.iter().any(|block| matches!(block, ContentBlock::Image { .. }))
}

#[tokio::test]
async fn returns_text_plus_images_in_final_output() {
    let mut server = MockServer::start(MockResponse::json(&image_response())).await;
    let mut model = model(&server, "google/gemini-3.1-flash-image-preview", &["text", "image"]);
    model.headers = BTreeMap::from([("HTTP-Referer".to_string(), "https://example.com".to_string())]);
    let output = generate_images(&model, &prompt("Generate a dog"), with_key("test")).await;

    assert_eq!(output.stop_reason, ImagesStopReason::Stop);
    assert_eq!(output.response_id.as_deref(), Some("img-1"));
    assert_eq!(output.output[0], ContentBlock::text("Here is your image."));
    assert_eq!(output.output[1], ContentBlock::Image { data: "ZmFrZS1wbmc=".into(), mime_type: "image/png".into() });
    let usage = output.usage.expect("usage");
    assert_eq!((usage.input, usage.output, usage.total_tokens), (12, 34, 46));
    assert!((usage.cost.total - (0.015 / 1e6 * 12.0 + 0.03 / 1e6 * 34.0)).abs() < 1e-15);

    let request = server.request().await;
    assert_eq!(request.path, "/api/v1/chat/completions");
    assert_eq!(
        (request.body["stream"].as_bool(), &request.body["modalities"]),
        (Some(false), &json!(["image", "text"]))
    );
    assert_eq!(request.body["messages"][0]["content"][0], json!({ "type": "text", "text": "Generate a dog" }));
    assert_eq!(request.headers["authorization"], "Bearer test");
    assert_eq!(request.headers["http-referer"], "https://example.com");
}

#[tokio::test]
async fn passes_through_cancellation_and_returns_aborted_result() {
    let server = MockServer::start(MockResponse::json(&image_response())).await;
    let options = with_key("test");
    options.request.cancel.cancel();
    let output =
        generate_images(&model(&server, "black-forest-labs/flux.2-pro", &["image"]), &prompt("Dog"), options).await;
    assert_eq!(output.stop_reason, ImagesStopReason::Aborted);
    assert_eq!(output.error_message.as_deref(), Some("Request aborted"));
}

#[tokio::test]
async fn image_only_models_request_only_image_output() {
    let mut server = MockServer::start(MockResponse::json(&image_response())).await;
    let model = model(&server, "black-forest-labs/flux.2-pro", &["image"]);
    let output = generate_images(&model, &prompt("Generate a dog"), with_key("test")).await;
    assert!(has_image(&output));
    assert_eq!(server.request().await.body["modalities"], json!(["image"]));
}

#[tokio::test]
async fn sends_image_input_as_data_urls_and_reads_object_image_urls() {
    let response = json!({
        "id": "img-2",
        "choices": [{
            "message": {
                "content": "",
                "images": [
                    { "image_url": { "url": "data:image/jpeg;base64,Ymx1ZQ==" } },
                    { "image_url": "https://example.com/remote.png" },
                    { "image_url": "data:image/png;base64,bad\nline" },
                ],
            },
        }],
    });
    let mut server = MockServer::start(MockResponse::json(&response)).await;
    let context = ImagesContext {
        input: vec![
            ContentBlock::text("Create a variation of this image with a blue background."),
            ContentBlock::Image { data: "cmVk".into(), mime_type: "image/png".into() },
        ],
    };
    let output =
        generate_images(&model(&server, "google/gemini-2.5-flash-image", &["text", "image"]), &context, with_key("k"))
            .await;

    assert_eq!(output.stop_reason, ImagesStopReason::Stop);
    assert_eq!(output.output, [ContentBlock::Image { data: "Ymx1ZQ==".into(), mime_type: "image/jpeg".into() }]);
    assert_eq!(output.usage, None);
    let content = &server.request().await.body["messages"][0]["content"];
    assert_eq!(content[1], json!({ "type": "image_url", "image_url": { "url": "data:image/png;base64,cmVk" } }));
}

#[tokio::test]
async fn splits_cached_prompt_tokens_from_input() {
    let usage = json!({
        "prompt_tokens": 100,
        "completion_tokens": 5,
        "prompt_tokens_details": { "cached_tokens": 30, "cache_write_tokens": 10 },
    });
    let server = MockServer::start(MockResponse::json(&json!({ "id": "img-3", "usage": usage, "choices": [] }))).await;
    let output = generate_images(&model(&server, "m", &["image"]), &prompt("x"), with_key("k")).await;
    let usage = output.usage.expect("usage");
    assert_eq!(
        (usage.input, usage.cache_read, usage.cache_write, usage.output, usage.total_tokens),
        (70, 20, 10, 5, 105)
    );
}

#[tokio::test]
async fn formats_http_failures_like_the_openai_sdk() {
    let server = MockServer::sequence(vec![
        MockResponse::Status(401, r#"{"error":{"message":"bad key","code":401}}"#.into()),
        MockResponse::Status(502, "upstream down".into()),
        MockResponse::Status(400, r#"{"message":"no error field"}"#.into()),
    ])
    .await;
    let model = model(&server, "m", &["image"]);
    let mut messages = Vec::new();
    for _ in 0..3 {
        let output = generate_images(&model, &prompt("x"), with_key("k")).await;
        assert_eq!(output.stop_reason, ImagesStopReason::Error);
        messages.push(output.error_message.unwrap());
    }
    assert_eq!(
        messages,
        [r#"401: {"message":"bad key","code":401}"#, "502 upstream down", "400 status code (no body)"]
    );
}

#[tokio::test]
async fn rejects_non_image_models_missing_keys_and_unknown_apis() {
    let server = MockServer::sequence(Vec::new()).await;
    let mut chat = model(&server, "chat", &["image"]);
    chat.model_type = None;
    let output = generate_images(&chat, &prompt("x"), with_key("k")).await;
    assert_eq!(output.error_message.as_deref(), Some("Model openrouter/chat is not an image model"));

    let mut keyless = model(&server, "m", &["image"]);
    keyless.provider = "local-images".into();
    let output = generate_images(&keyless, &prompt("x"), ImagesOptions::default()).await;
    assert_eq!(output.error_message.as_deref(), Some("No API key for provider: local-images"));

    let mut unknown = model(&server, "m", &["image"]);
    unknown.api = "unknown-images".into();
    let output = generate_images(&unknown, &prompt("x"), with_key("k")).await;
    assert_eq!(output.error_message.as_deref(), Some("No API provider registered for api: unknown-images"));
    assert!(server.requests().is_empty());
    assert!(images_api("openrouter-images").is_some());
}
