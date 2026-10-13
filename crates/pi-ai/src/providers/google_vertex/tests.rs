use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ring::signature::{KeyPair, RSA_PKCS1_2048_8192_SHA256, RsaKeyPair, UnparsedPublicKey};
use serde_json::{Value, json};

use super::auth::sign_assertion;
use super::{GoogleVertex, includes_api_version, resolve_api_key, vertex_url};
use crate::providers::mock_http::{MockHttp, Reply};
use crate::providers::mock_server::{collect, frame, test_model};
use crate::registry::{ApiProvider, StreamOptions};
use crate::types::{Context, Message, Model, StopReason};

const API: &str = "google-vertex";

fn test_key_pem() -> String {
    format!("-----BEGIN PRIVATE KEY-----\n{}-----END PRIVATE KEY-----\n", include_str!("test_key.b64"))
}

fn ok_stream() -> Reply {
    Reply::sse(vec![frame(
        None,
        &json!({"responseId": "vertex-1", "candidates": [{"content": {"parts": [{"text": "ok"}]}, "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1, "totalTokenCount": 2}}),
    )])
}

fn token_reply(token: &str) -> Reply {
    Reply::json(200, &json!({"access_token": token, "expires_in": 3599, "token_type": "Bearer"}))
}

fn model(base_url: &str) -> Model {
    Model { id: "gemini-3-flash-preview".into(), ..test_model(API, "google-vertex", base_url) }
}

fn context() -> Context {
    Context { messages: vec![Message::user("hello")], ..Context::default() }
}

fn adc_options(credentials: &std::path::Path) -> StreamOptions {
    let mut options = StreamOptions::default();
    options.env.insert("GOOGLE_CLOUD_PROJECT".into(), "test-project".into());
    options.env.insert("GOOGLE_CLOUD_LOCATION".into(), "us-central1".into());
    options.env.insert("GOOGLE_APPLICATION_CREDENTIALS".into(), credentials.display().to_string());
    options
}

fn write_json(dir: &tempfile::TempDir, name: &str, value: &Value) -> std::path::PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, value.to_string()).unwrap();
    path
}

#[tokio::test]
async fn api_keys_use_express_mode_and_custom_base_urls() {
    let server = MockHttp::start(vec![ok_stream()]).await;
    let options = StreamOptions { api_key: Some("AIzaSyExample".into()), ..StreamOptions::default() };
    let (_, message) = collect(GoogleVertex.stream(&model(&server.base_url), &context(), options)).await;
    assert_eq!((message.stop_reason, message.text().as_str()), (StopReason::Stop, "ok"));
    assert_eq!(message.response_id.as_deref(), Some("vertex-1"));
    let request = server.request(0);
    assert_eq!(request.path, "/v1/publishers/google/models/gemini-3-flash-preview:streamGenerateContent?alt=sse");
    assert_eq!(request.headers["x-goog-api-key"], "AIzaSyExample");
    assert!(!request.headers.contains_key("authorization"));
    for marker in ["<authenticated>", "gcp-vertex-credentials", "  "] {
        let options = StreamOptions { api_key: Some(marker.into()), ..StreamOptions::default() };
        assert_eq!(resolve_api_key(&options), None, "{marker}");
    }
}

#[test]
fn urls_follow_the_sdk_endpoint_rules() {
    let scope = ("p".to_string(), "us-central1".to_string());
    let path = "publishers/google/models/m";
    let template = Model { base_url: "https://{location}-aiplatform.googleapis.com".into(), ..Model::default() };
    assert_eq!(
        vertex_url(&template, Some(&scope), path),
        "https://us-central1-aiplatform.googleapis.com/v1/projects/p/locations/us-central1/publishers/google/models/m:streamGenerateContent?alt=sse"
    );
    let global = ("p".to_string(), "global".to_string());
    assert!(
        vertex_url(&template, Some(&global), path)
            .starts_with("https://aiplatform.googleapis.com/v1/projects/p/locations/global/")
    );
    let us = ("p".to_string(), "us".to_string());
    assert!(
        vertex_url(&template, Some(&us), path).starts_with("https://aiplatform.us.rep.googleapis.com/v1/projects/p/")
    );
    assert_eq!(
        vertex_url(&Model::default(), None, path),
        "https://aiplatform.googleapis.com/v1/publishers/google/models/m:streamGenerateContent?alt=sse"
    );
    let versioned =
        Model { base_url: "https://proxy.example.com/v1/projects/p/locations/global".into(), ..Model::default() };
    assert_eq!(
        vertex_url(&versioned, Some(&scope), path),
        "https://proxy.example.com/v1/projects/p/locations/global/publishers/google/models/m:streamGenerateContent?alt=sse"
    );
    let tuned = "projects/p/locations/l/endpoints/e";
    assert!(vertex_url(&template, Some(&scope), tuned).contains("/v1/projects/p/locations/l/endpoints/e:"));
    assert!(includes_api_version("https://x.example.com/v1beta1/models"));
    assert!(!includes_api_version("https://x.example.com/v1x"));
}

#[tokio::test]
async fn service_accounts_sign_an_rs256_assertion() {
    let token_server = MockHttp::start(vec![token_reply("sa-token")]).await;
    let api = MockHttp::start(vec![ok_stream()]).await;
    let dir = tempfile::tempdir().unwrap();
    let credentials = json!({
        "type": "service_account", "project_id": "p", "private_key_id": "k1", "private_key": test_key_pem(),
        "client_email": "bot@p.iam.gserviceaccount.com", "token_uri": format!("{}/token", token_server.base_url),
        "quota_project_id": "billing-project"
    });
    let path = write_json(&dir, "sa.json", &credentials);
    let options = StreamOptions { api_key: Some("<authenticated>".into()), ..adc_options(&path) };
    let (_, message) = collect(GoogleVertex.stream(&model(&api.base_url), &context(), options)).await;
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    let request = api.request(0);
    assert_eq!(request.path, "/v1/publishers/google/models/gemini-3-flash-preview:streamGenerateContent?alt=sse");
    assert_eq!(request.headers["authorization"], "Bearer sa-token");
    assert_eq!(request.headers["x-goog-user-project"], "billing-project");

    let form = token_server.request(0).form();
    assert_eq!(form["grant_type"], "urn:ietf:params:oauth:grant-type:jwt-bearer");
    let assertion = &form["assertion"];
    let segments: Vec<&str> = assertion.split('.').collect();
    let decode = |segment: &str| serde_json::from_slice::<Value>(&URL_SAFE_NO_PAD.decode(segment).unwrap()).unwrap();
    assert_eq!(decode(segments[0]), json!({"alg": "RS256"}));
    let claims = decode(segments[1]);
    assert_eq!(claims["iss"], "bot@p.iam.gserviceaccount.com");
    assert_eq!(claims["scope"], "https://www.googleapis.com/auth/cloud-platform");
    assert_eq!(claims["aud"], format!("{}/token", token_server.base_url));
    assert_eq!(claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap(), 3600);
    let der = STANDARD.decode(include_str!("test_key.b64").split_whitespace().collect::<String>()).unwrap();
    let public = RsaKeyPair::from_pkcs8(&der).unwrap().public_key().as_ref().to_vec();
    let signature = URL_SAFE_NO_PAD.decode(segments[2]).unwrap();
    let signed = format!("{}.{}", segments[0], segments[1]);
    UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, public).verify(signed.as_bytes(), &signature).unwrap();
}

#[tokio::test]
async fn authorized_users_refresh_from_the_gcloud_well_known_file() {
    let token_server = MockHttp::start(vec![token_reply("user-token")]).await;
    let api = MockHttp::start(vec![ok_stream(), ok_stream()]).await;
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".config/gcloud")).unwrap();
    let credentials = json!({
        "type": "authorized_user", "client_id": "cid", "client_secret": "secret", "refresh_token": "refresh",
        "token_uri": format!("{}/token", token_server.base_url)
    });
    std::fs::write(home.path().join(".config/gcloud/application_default_credentials.json"), credentials.to_string())
        .unwrap();
    let mut options = adc_options(std::path::Path::new(""));
    options.env.remove("GOOGLE_APPLICATION_CREDENTIALS");
    options.env.insert("HOME".into(), home.path().display().to_string());
    options.env.insert("APPDATA".into(), home.path().join(".config").display().to_string());
    if std::env::var("GOOGLE_APPLICATION_CREDENTIALS").is_ok_and(|value| !value.is_empty()) {
        return;
    }
    for _ in 0..2 {
        let (_, message) = collect(GoogleVertex.stream(&model(&api.base_url), &context(), options.clone())).await;
        assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    }
    assert_eq!(token_server.requests().len(), 1, "the access token is cached");
    let form = token_server.request(0).form();
    assert_eq!((form["grant_type"].as_str(), form["refresh_token"].as_str()), ("refresh_token", "refresh"));
    assert_eq!((form["client_id"].as_str(), form["client_secret"].as_str()), ("cid", "secret"));
    assert_eq!(api.request(1).headers["authorization"], "Bearer user-token");
}

#[tokio::test]
async fn metadata_server_tokens_and_missing_credentials() {
    let metadata = MockHttp::start(vec![token_reply("gce-token")]).await;
    let api = MockHttp::start(vec![ok_stream()]).await;
    let empty_home = tempfile::tempdir().unwrap();
    let mut options = adc_options(std::path::Path::new(""));
    options.env.remove("GOOGLE_APPLICATION_CREDENTIALS");
    options.env.insert("HOME".into(), empty_home.path().display().to_string());
    options.env.insert("APPDATA".into(), empty_home.path().display().to_string());
    if std::env::var("GOOGLE_APPLICATION_CREDENTIALS").is_ok_and(|value| !value.is_empty()) {
        return;
    }
    let mut gce = options.clone();
    gce.env.insert("GCE_METADATA_HOST".into(), metadata.base_url.trim_start_matches("http://").into());
    let (_, message) = collect(GoogleVertex.stream(&model(&api.base_url), &context(), gce)).await;
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    let token_request = metadata.request(0);
    assert_eq!(token_request.method, "GET");
    assert!(token_request.path.starts_with("/computeMetadata/v1/instance/service-accounts/default/token"));
    assert_eq!(token_request.headers["metadata-flavor"], "Google");
    assert_eq!(api.request(0).headers["authorization"], "Bearer gce-token");

    options.env.insert("METADATA_SERVER_DETECTION".into(), "none".into());
    let (_, message) = collect(GoogleVertex.stream(&model(&api.base_url), &context(), options)).await;
    assert!(message.error_message.unwrap().starts_with("Could not load the default credentials."));
}

#[tokio::test]
async fn adc_requires_project_and_location() {
    let mut options = StreamOptions::default();
    if std::env::var("GOOGLE_CLOUD_PROJECT").is_ok() || std::env::var("GCLOUD_PROJECT").is_ok() {
        return;
    }
    let (_, message) = collect(GoogleVertex.stream(&model("http://127.0.0.1:9"), &context(), options.clone())).await;
    assert_eq!(
        message.error_message.as_deref(),
        Some("Vertex AI requires a project ID. Set GOOGLE_CLOUD_PROJECT/GCLOUD_PROJECT or pass project in options.")
    );
    options.env.insert("GCLOUD_PROJECT".into(), "p".into());
    if std::env::var("GOOGLE_CLOUD_LOCATION").is_err() {
        let (_, message) = collect(GoogleVertex.stream(&model("http://127.0.0.1:9"), &context(), options)).await;
        assert_eq!(
            message.error_message.as_deref(),
            Some("Vertex AI requires a location. Set GOOGLE_CLOUD_LOCATION or pass location in options.")
        );
    }
}

#[tokio::test]
async fn safety_stops_are_errors_and_bad_keys_fail_cleanly() {
    let server =
        MockHttp::start(vec![Reply::sse(vec![frame(None, &json!({"candidates": [{"finishReason": "SAFETY"}]}))])])
            .await;
    let options = StreamOptions { api_key: Some("AIzaSyExample".into()), ..StreamOptions::default() };
    let (_, message) = collect(GoogleVertex.stream(&model(&server.base_url), &context(), options)).await;
    assert_eq!((message.stop_reason, message.raw_stop_reason.as_deref()), (StopReason::Error, Some("SAFETY")));
    assert_eq!(message.error_message.as_deref(), Some("Provider stopped with: SAFETY"));
    assert!(sign_assertion("a@b", "not a pem", "aud", 0).unwrap_err().contains("PEM"));
}

#[tokio::test]
async fn streams_thinking_tool_calls_images_usage_and_aborts() {
    let parts = |parts: Value| frame(None, &json!({"candidates": [{"content": {"role": "model", "parts": parts}}]}));
    let server = MockHttp::start(vec![Reply::sse(vec![
        parts(json!([{"text": "Look", "thought": true, "thoughtSignature": "c2ln"}])),
        parts(json!([{"functionCall": {"id": "call-1", "name": "read", "args": {"path": "a.png"}}}])),
        frame(None, &json!({"candidates": [{"finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 50, "candidatesTokenCount": 5, "thoughtsTokenCount": 5, "totalTokenCount": 60}})),
    ])])
    .await;
    let user = Message::User(crate::types::UserMessage {
        content: crate::types::UserContent::Blocks(vec![
            crate::types::ContentBlock::text("what is this"),
            crate::types::ContentBlock::Image { data: "aGVsbG8=".into(), mime_type: "image/png".into() },
        ]),
        timestamp: 1,
    });
    let context = Context { messages: vec![user], ..Context::default() };
    let options = StreamOptions {
        api_key: Some("AIzaSyExample".into()),
        reasoning: Some(crate::types::ThinkingLevel::High),
        ..StreamOptions::default()
    };
    let (_, message) = collect(GoogleVertex.stream(&model(&server.base_url), &context, options)).await;
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert!(matches!(&message.content[0], crate::types::ContentBlock::Thinking { thinking, .. } if thinking == "Look"));
    assert_eq!(message.tool_calls().next().unwrap().arguments["path"], "a.png");
    assert_eq!((message.usage.input, message.usage.output, message.usage.total_tokens), (50, 10, 60));
    assert!((message.usage.cost.total - (50.0 + 10.0 * 2.0) / 1e6).abs() < 1e-12);
    let body = server.request(0).json();
    assert_eq!(body["contents"][0]["parts"][1]["inlineData"]["mimeType"], "image/png");
    assert_eq!(body["generationConfig"]["thinkingConfig"], json!({"includeThoughts": true, "thinkingLevel": "HIGH"}));

    let hanging = Reply {
        body: crate::providers::mock_http::Body::ChunksThenHang(vec![parts(json!([{"text": "par"}]))]),
        ..Reply::sse(Vec::new())
    };
    let server = MockHttp::start(vec![hanging]).await;
    let options = StreamOptions { api_key: Some("AIzaSyExample".into()), ..StreamOptions::default() };
    let cancel = options.cancel.clone();
    let stream = GoogleVertex.stream(&model(&server.base_url), &context, options);
    let message = crate::providers::mock_server::cancel_after_first_text(stream, cancel).await;
    assert_eq!(message.stop_reason, StopReason::Aborted);
}

#[tokio::test]
async fn external_accounts_authenticate_vertex_requests() {
    if std::env::var("GOOGLE_CLOUD_QUOTA_PROJECT").is_ok_and(|value| !value.is_empty()) {
        return;
    }
    let sts = MockHttp::start(vec![token_reply("federated-token")]).await;
    let api = MockHttp::start(vec![ok_stream(), ok_stream()]).await;
    let dir = tempfile::tempdir().unwrap();
    let subject = dir.path().join("subject.txt");
    std::fs::write(&subject, "oidc-token").unwrap();
    let credentials = json!({
        "type": "external_account", "subject_token_type": "urn:ietf:params:oauth:token-type:jwt",
        "audience": "//iam.googleapis.com/projects/1/locations/global/workloadIdentityPools/p/providers/x",
        "token_url": format!("{}/v1/token", sts.base_url), "credential_source": {"file": subject},
        "quota_project_id": "federated-quota"
    });
    let options = adc_options(&write_json(&dir, "external.json", &credentials));
    for _ in 0..2 {
        let (_, message) = collect(GoogleVertex.stream(&model(&api.base_url), &context(), options.clone())).await;
        assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    }
    assert_eq!(sts.requests().len(), 1, "the federated token is cached");
    assert_eq!(sts.request(0).form()["subject_token"], "oidc-token");
    let request = api.request(1);
    assert_eq!(request.headers["authorization"], "Bearer federated-token");
    assert_eq!(request.headers["x-goog-user-project"], "federated-quota");
}
