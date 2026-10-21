//! Port of upstream `test/identity.test.ts` ("client identity in requests") over `openai-completions`.

use std::collections::BTreeMap;

use super::OpenAICompletions;
use super::test_support::{API, ok_frames, options, user_context};
use crate::identity::{ClientIdentity, UserAgentMode, pi_attribution};
use crate::providers::mock_server::{MockResponse, MockServer, RecordedRequest, collect, test_model};
use crate::registry::{ApiProvider, StreamOptions};
use crate::types::Model;

async fn send(model: &Model, options: StreamOptions, registry: bool) -> RecordedRequest {
    let mut server = MockServer::start(MockResponse::Sse(ok_frames())).await;
    let model = Model { base_url: server.base_url.clone(), ..model.clone() };
    let context = user_context("hello");
    let stream = if registry {
        crate::registry::stream(&model, &context, options)
    } else {
        OpenAICompletions.stream(&model, &context, options)
    };
    let (_, message) = collect(stream).await;
    assert_eq!(message.error_message, None);
    server.request().await
}

fn headers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(name, value)| (name.to_string(), value.to_string())).collect()
}

#[tokio::test]
async fn applies_the_identity_below_model_and_request_headers() {
    let mut model = test_model(API, "openrouter", "");
    model.headers = headers(&[("X-OpenRouter-Title", "model-title")]);
    let identity =
        ClientIdentity { user_agent: Some(UserAgentMode::Short), ..ClientIdentity::attributed(pi_attribution()) };
    let options =
        StreamOptions { identity: Some(identity), suppressed_headers: vec!["HTTP-Referer".into()], ..options() };
    let request = send(&model, options, false).await;
    assert_eq!(request.headers["user-agent"], "pi");
    assert_eq!(request.headers["x-openrouter-title"], "model-title");
    assert_eq!(request.headers["x-openrouter-categories"], "cli-agent");
    assert!(!request.headers.contains_key("http-referer"));
}

#[tokio::test]
async fn sends_no_attribution_by_default() {
    let request = send(&test_model(API, "openrouter", ""), options(), false).await;
    assert_eq!(request.headers["user-agent"], crate::identity::format_user_agent(None));
    assert!(!request.headers.contains_key("x-openrouter-title"));
}

#[tokio::test]
async fn keeps_impersonating_model_headers_over_the_identity_user_agent() {
    let mut model = test_model(API, "github-copilot", "");
    model.headers = headers(&[("User-Agent", "GitHubCopilotChat/0.35.0")]);
    let identity = ClientIdentity { user_agent: Some(UserAgentMode::Custom("acme".into())), ..Default::default() };
    let request = send(&model, StreamOptions { identity: Some(identity), ..options() }, false).await;
    assert_eq!(request.headers["user-agent"], "GitHubCopilotChat/0.35.0");
}

#[tokio::test]
async fn sends_the_opencode_session_header_and_lets_configured_headers_win() {
    let model = test_model(API, "opencode", "");
    let session = StreamOptions { session_id: Some("session-1".into()), ..options() };
    let request = send(&model, session.clone(), true).await;
    assert_eq!(request.headers["x-opencode-session"], "session-1");
    assert!(!request.headers.contains_key("x-opencode-client"), "no attribution without an identity");
    let configured = StreamOptions { headers: headers(&[("X-OpenCode-Session", "configured")]), ..session };
    assert_eq!(send(&model, configured, true).await.headers["x-opencode-session"], "configured");
}
