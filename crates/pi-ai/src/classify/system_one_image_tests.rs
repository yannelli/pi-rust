//! Image input for System One classifiers, ported from upstream `typesafe-system-one.test.ts`
//! and `cloudflare-workers-ai-system-one.test.ts`.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::test_support::{classifier_model, context, with_key};
use super::{
    ClassifierApi, ClassifierContext, ClassifierOptions, ClassifierStopReason, CloudflareWorkersAISystemOne,
    ImageContent, TypeSafeSystemOne,
};
use crate::providers::mock_server::{MockResponse, MockServer, RecordedRequest};
use crate::types::Model;

const OPT_IN: &str = "typesafe-danger-zone-images";

fn image(data: &str, mime_type: &str) -> ImageContent {
    ImageContent { data: data.into(), mime_type: mime_type.into() }
}

fn sample_context(images: Vec<ImageContent>) -> ClassifierContext {
    let mut context = context(json!({
        "state": { "text": "The deployment succeeded." },
        "questions": { "approved": { "type": "bool", "instructions": "Approve?", "criteria": { "true": "Yes", "false": "No" } } },
    }));
    context.images = Some(images).filter(|images| !images.is_empty());
    context
}

fn state() -> Value {
    json!({ "text": "The deployment succeeded." })
}

fn typesafe(base_url: &str, id: &str, provider: &str) -> Model {
    let mut model = classifier_model("typesafe-system-one", provider, id, base_url);
    model.input.push("image".into());
    model
}

fn answers() -> Value {
    json!({ "answers": { "approved": { "type": "noul", "noul": 0.8 } } })
}

/// Classify with `model` against a server that answers every request, and return the requests.
async fn send(api: &dyn ClassifierApi, model: &Model, contexts: &[ClassifierContext]) -> Vec<RecordedRequest> {
    let response = match model.api.as_str() {
        "typesafe-system-one" => answers(),
        _ => json!({ "result": answers(), "success": true, "errors": [], "messages": [] }),
    };
    let server = MockServer::sequence(vec![MockResponse::json(&response); contexts.len()]).await;
    let model = Model { base_url: model.base_url.replace("{server}", &server.base_url), ..model.clone() };
    for context in contexts {
        let options = ClassifierOptions { request: cloudflare_env(), ..with_key("secret") };
        let result = api.classify(&model, context, options).await;
        assert_eq!(result.stop_reason, ClassifierStopReason::Stop, "{:?}", result.error_message);
    }
    server.requests()
}

fn cloudflare_env() -> crate::request::RequestOptions {
    let env = BTreeMap::from([("CLOUDFLARE_ACCOUNT_ID".to_string(), "account-id".to_string())]);
    crate::request::RequestOptions { api_key: Some("secret".into()), env, ..Default::default() }
}

#[tokio::test]
async fn sends_images_to_typesafe_as_assets_with_the_opt_in_header() {
    let model = typesafe("{server}/v1", "jev-latest", "typesafe");
    let images = vec![image("cG5n", "image/png"), image("anBlZw==", "image/jpeg")];
    let requests = send(&TypeSafeSystemOne, &model, &[sample_context(images), sample_context(Vec::new())]).await;

    assert_eq!(requests[0].body["state"], json!(["<image-1>", "<image-2>", state()]));
    assert_eq!(
        requests[0].body["assets"],
        json!({
            "<image-1>": { "type": "image", "format": "png", "data_base64": "cG5n" },
            "<image-2>": { "type": "image", "format": "jpeg", "data_base64": "anBlZw==" },
        })
    );
    assert_eq!(requests[0].headers.get(OPT_IN).map(String::as_str), Some("true"));
    // Requests without images keep the plain state and send no opt-in header.
    assert_eq!(requests[1].body["state"], state());
    assert!(requests[1].body.get("assets").is_none() && !requests[1].headers.contains_key(OPT_IN));
}

#[tokio::test]
async fn rejects_text_only_models_and_formats_typesafe_does_not_accept_before_sending() {
    let server = MockServer::sequence(Vec::new()).await;
    let images = sample_context(vec![image("d2VicA==", "image/webp")]);
    let text_only = classifier_model("typesafe-system-one", "typesafe", "jev-latest", &server.base_url);
    let result = TypeSafeSystemOne.classify(&text_only, &images, with_key("secret")).await;
    assert_eq!(result.error_message.as_deref(), Some("System One API does not support image input for jev-latest"));
    let result = TypeSafeSystemOne
        .classify(&typesafe(&server.base_url, "jev-latest", "typesafe"), &images, with_key("secret"))
        .await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Error);
    assert_eq!(
        result.error_message.as_deref(),
        Some("System One API accepts PNG and JPEG images for jev-latest, got image/webp")
    );
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn sends_openrouter_s_image_form_as_image_url_parts_in_the_state() {
    let proxy_compat = json!({ "imageInput": "state-image-urls", "sendImageOptInHeader": false });
    let models = [
        typesafe("{server}/api/v1", "typesafe/jev-1.14", "openrouter"),
        typesafe("{server}/us.openrouter.ai/api/v1", "typesafe/jev-1.14", "custom"),
        Model { compat: Some(proxy_compat), ..typesafe("{server}/v1", "typesafe/jev-1.14", "custom") },
    ];
    for model in models {
        let requests = send(&TypeSafeSystemOne, &model, &[sample_context(vec![image("cG5n", "image/png")])]).await;
        // OpenRouter answers 200 for `assets` but drops them, and rejects a top-level `images` list.
        let part = json!({ "type": "image_url", "image_url": { "url": "data:image/png;base64,cG5n" } });
        assert_eq!(requests[0].body["state"], json!([part, state()]), "{}", model.base_url);
        assert_eq!(requests[0].body["model"], "typesafe/jev-1.14");
        assert!(requests[0].body.get("assets").is_none() && requests[0].body.get("images").is_none());
        assert!(!requests[0].headers.contains_key(OPT_IN));
    }
}

#[tokio::test]
async fn compat_overrides_detected_image_settings_per_field() {
    let model = Model {
        compat: Some(json!({ "sendImageOptInHeader": false })),
        ..typesafe("{server}/v1", "jev-latest", "typesafe")
    };
    let requests = send(&TypeSafeSystemOne, &model, &[sample_context(vec![image("cG5n", "image/png")])]).await;
    assert!(requests[0].body.get("assets").is_some());
    assert!(!requests[0].headers.contains_key(OPT_IN));
}

fn workers_ai(id: &str) -> Model {
    let base_url = "{server}/client/v4/accounts/{CLOUDFLARE_ACCOUNT_ID}/ai";
    let mut model = classifier_model("cloudflare-workers-ai-system-one", "cloudflare-workers-ai", id, base_url);
    model.input.push("image".into());
    model
}

#[tokio::test]
async fn sends_images_to_clef_as_data_urls() {
    let images = vec![image("cG5n", "image/png"), image("d2VicA==", "image/webp")];
    let requests =
        send(&CloudflareWorkersAISystemOne, &workers_ai("@cf/cloudflare/clef"), &[sample_context(images)]).await;
    // Clef rejects `assets` with a 422, so the state stays as is.
    let input = &requests[0].body["input"];
    assert_eq!(input["state"], state());
    assert_eq!(input["images"], json!(["data:image/png;base64,cG5n", "data:image/webp;base64,d2VicA=="]));
    assert!(input.get("assets").is_none() && !requests[0].headers.contains_key(OPT_IN));

    // A Clef deployment under an ID the prefix check does not recognize reads the form from compat.
    let custom = Model { compat: Some(json!({ "imageInput": "images" })), ..workers_ai("@cf/acme/clef-finetune") };
    let requests =
        send(&CloudflareWorkersAISystemOne, &custom, &[sample_context(vec![image("cG5n", "image/png")])]).await;
    assert_eq!(requests[0].body["input"]["images"], json!(["data:image/png;base64,cG5n"]));
    assert!(requests[0].body["input"].get("assets").is_none());
}

#[tokio::test]
async fn rejects_more_than_four_images_for_clef_before_sending() {
    let server = MockServer::sequence(Vec::new()).await;
    let model = Model { base_url: server.base_url.clone(), ..workers_ai("@cf/cloudflare/clef-flash") };
    let context = sample_context(vec![image("cG5n", "image/png"); 5]);
    let result = CloudflareWorkersAISystemOne.classify(&model, &context, with_key("secret")).await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Error);
    assert_eq!(
        result.error_message.as_deref(),
        Some("Cloudflare Workers AI accepts at most 4 images for @cf/cloudflare/clef-flash, got 5")
    );
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn sends_images_to_jev_on_workers_ai_as_system_one_assets() {
    for id in ["typesafe/jev", "typesafe/jev-1.14.0"] {
        let run = json!({ "result": { "state": "Completed", "result": answers() }, "success": true });
        let server = MockServer::sequence(vec![MockResponse::json(&run)]).await;
        let model = Model { base_url: workers_ai(id).base_url.replace("{server}", &server.base_url), ..workers_ai(id) };
        let context = sample_context(vec![image("cG5n", "image/png")]);
        let options = ClassifierOptions { request: cloudflare_env(), ..with_key("secret") };
        let result = CloudflareWorkersAISystemOne.classify(&model, &context, options).await;
        assert_eq!(result.stop_reason, ClassifierStopReason::Stop, "{:?}", result.error_message);
        let request = &server.requests()[0];
        assert_eq!(request.body["model"], id);
        assert_eq!(request.body["input"]["state"], json!(["<image-1>", state()]));
        assert_eq!(
            request.body["input"]["assets"],
            json!({ "<image-1>": { "type": "image", "format": "png", "data_base64": "cG5n" } })
        );
        // Workers AI needs no TypeSafe opt-in header.
        assert!(!request.headers.contains_key(OPT_IN));
    }
}
