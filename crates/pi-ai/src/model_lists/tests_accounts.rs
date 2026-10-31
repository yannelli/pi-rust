//! pi-rust's account model lists against local servers: filter, enrich, add, and fallbacks.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::json;

use super::Personalized;
use super::reconcile::{AddRule, Listed, reconcile};
use super::tests_openrouter::serve;
use super::*;
use crate::providers::http_mock::{Reply, RouteServer};

pub(super) fn chat(provider: &str, api: &str, id: &str) -> Model {
    serde_json::from_value(json!({"id": id, "name": id, "api": api, "provider": provider,
        "baseUrl": "https://example.test/v1", "reasoning": true, "input": ["text"], "contextWindow": 100_000,
        "maxTokens": 10_000, "headers": {"x-static": "1"}, "compat": {"flag": id},
        "cost": {"input": 1, "output": 2, "cacheRead": 0, "cacheWrite": 0}}))
    .unwrap()
}

pub(super) fn classifier(provider: &str, id: &str) -> Model {
    Model { model_type: Some("classifier".into()), ..chat(provider, "openai-decisions", id) }
}

/// The models of a list that narrowed the catalog, to save.
pub(super) trait Saved {
    fn saved(self) -> Vec<Model>;
}

impl Saved for Personalized {
    fn saved(self) -> Vec<Model> {
        match self {
            Personalized::Models(models) => models,
            unmatched => panic!("expected models to save: {unmatched:?}"),
        }
    }
}

impl Saved for Result<Personalized, String> {
    fn saved(self) -> Vec<Model> {
        self.unwrap().saved()
    }
}

pub(super) fn ids(models: &[Model]) -> Vec<&str> {
    models.iter().map(|model| model.id.as_str()).collect()
}

pub(super) fn context(server: &RouteServer, key: Option<&str>) -> ModelListContext {
    ModelListContext {
        base_url: Some(server.base_url.clone()),
        client: reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).build().unwrap(),
        ..ModelListContext::new(key.map(String::from))
    }
}

/// Every rejecting status and an unreadable body leave `catalog` unchanged; other failing
/// statuses are errors.
pub(super) async fn assert_fallbacks(list: AccountModelList, key: &str, catalog: Vec<Model>) {
    for status in [401, 403, 404] {
        let server = serve(status, json!({"error": "no"})).await;
        let result = list.personalize(catalog.clone(), &context(&server, Some(key))).await;
        assert_eq!(result.saved(), catalog, "{list:?} status {status}");
    }
    let server = RouteServer::start(|_| Reply::new(200, "application/json", "{ not json")).await;
    let result = list.personalize(catalog.clone(), &context(&server, Some(key))).await;
    assert_eq!(result.saved(), catalog, "{list:?} bad JSON");
    let server = serve(200, json!({"unexpected": true})).await;
    let result = list.personalize(catalog.clone(), &context(&server, Some(key))).await;
    assert_eq!(result.saved(), catalog, "{list:?} unknown shape");
    let server = serve(502, json!({})).await;
    assert!(list.personalize(catalog.clone(), &context(&server, Some(key))).await.is_err(), "{list:?} 502");
    let server = serve(200, json!({})).await;
    let result = list.personalize(catalog.clone(), &context(&server, None)).await;
    assert_eq!(result.saved(), catalog);
    assert!(server.requests().is_empty(), "{list:?} sends nothing without a key");
}

#[test]
fn reconcile_keeps_aliases_takes_limits_and_adds_copies_of_the_closest_model() {
    let catalog = vec![
        chat("p", "a", "claude-opus-4-5"),
        chat("p", "a", "claude-opus-4-5-20251101"),
        chat("p", "b", "claude-sonnet-4-6"),
        chat("p", "a", "claude-haiku-4-5"),
        classifier("p", "judge"),
    ];
    let listed = [
        Listed { context_window: Some(500_000), ..Listed::new("claude-opus-4-5-20251101") },
        Listed { name: Some("Sonnet 4.7".into()), max_tokens: Some(64_000), ..Listed::new("claude-sonnet-4-7") },
        Listed::new("claude-sonnet-4-7-20261001"),
    ];
    let models = reconcile(catalog, &listed, AddRule::Any, None).saved();

    assert_eq!(ids(&models), ["claude-opus-4-5", "claude-opus-4-5-20251101", "judge", "claude-sonnet-4-7"]);
    assert_eq!(models[0].context_window, 500_000, "an alias takes its snapshot's limits");
    let added = &models[3];
    assert_eq!((added.name.as_str(), added.api.as_str(), added.max_tokens), ("Sonnet 4.7", "b", 64_000));
    assert_eq!((added.context_window, added.base_url.as_str()), (100_000, "https://example.test/v1"));
    assert_eq!(added.compat, Some(json!({"flag": "claude-sonnet-4-6"})));
    assert_eq!(added.headers["x-static"], "1");
    let empty = reconcile(vec![chat("p", "a", "x")], &[], AddRule::Any, None);
    assert_eq!(empty, Personalized::Unmatched(vec![chat("p", "a", "x")]));
}

#[test]
fn catalog_families_add_only_chat_models_of_known_families() {
    let catalog = vec![chat("openai", "r", "gpt-5.4"), chat("openai", "r", "o3")];
    let listed: Vec<Listed> = [
        "gpt-5.4",
        "gpt-5.9",
        "o3-deep-research",
        "gpt-image-2",
        "text-embedding-3-large",
        "gpt-4o-mini-tts",
        "omni-moderation-latest",
        "gpt-realtime",
        "davinci-002",
        "ft:gpt-4o:org::x",
        "whisper-1",
    ]
    .into_iter()
    .map(Listed::new)
    .collect();
    let models = reconcile(catalog, &listed, AddRule::CatalogFamilies, None).saved();
    assert_eq!(ids(&models), ["gpt-5.4", "gpt-5.9", "o3-deep-research"]);
    assert_eq!(models[1].compat, Some(json!({"flag": "gpt-5.4"})));
}

fn codex_token(account: Option<&str>) -> String {
    let claims = match account {
        Some(account) => json!({"https://api.openai.com/auth": {"chatgpt_account_id": account}}),
        None => json!({"sub": "user"}),
    };
    format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(claims.to_string()))
}

fn codex_catalog() -> Vec<Model> {
    let codex = |id| chat("openai-codex", "openai-codex-responses", id);
    vec![codex("gpt-5.5"), codex("gpt-6-sol"), codex("gpt-6.1-sol"), codex("gpt-5.3-codex-spark")]
}

#[tokio::test]
async fn codex_lists_the_accounts_visible_models() {
    let body = json!({"models": [
        {"slug": "gpt-6-sol", "display_name": "GPT-6 Sol", "visibility": "list", "supported_in_api": true,
         "context_window": 400_000, "supported_reasoning_levels": [{"effort": "low"}], "extra": {"x": 1},
         "service_tiers": [{"id": "priority", "name": "Fast", "description": "1.5x speed, increased usage"}],
         "additional_speed_tiers": ["fast"]},
        {"slug": "gpt-6.1-sol", "visibility": "hide"},
        {"id": "gpt-6.2-sol", "display_name": "GPT-6.2 Sol"},
        {"slug": "gpt-5.5", "visibility": "LIST"},
        {"visibility": "list"},
    ]});
    let server = serve(200, body).await;
    let token = codex_token(Some("acct-test"));
    let models =
        AccountModelList::OpenAiCodex.personalize(codex_catalog(), &context(&server, Some(&token))).await.saved();

    assert_eq!(ids(&models), ["gpt-5.5", "gpt-6-sol", "gpt-6.2-sol"]);
    assert_eq!((models[1].context_window, models[1].name.as_str()), (400_000, "gpt-6-sol"));
    assert_eq!(models[1].compat, Some(json!({"flag": "gpt-6-sol", "serviceTiers": ["priority"]})));
    assert_eq!(models[0].compat, Some(json!({"flag": "gpt-5.5"})), "no service_tiers keeps compat");
    assert_eq!((models[2].name.as_str(), models[2].api.as_str()), ("GPT-6.2 Sol", "openai-codex-responses"));
    assert_eq!(models[2].compat, Some(json!({"flag": "gpt-6.1-sol"})), "copied from the closest model");
    let request = &server.requests()[0];
    assert_eq!(request.path, "/codex/models?client_version=0.166.0");
    assert_eq!(request.headers["authorization"], format!("Bearer {token}"));
    assert_eq!(
        (request.headers["chatgpt-account-id"].as_str(), request.headers["originator"].as_str()),
        ("acct-test", "pi")
    );
}

#[tokio::test]
async fn codex_falls_back_to_the_catalog() {
    assert_fallbacks(AccountModelList::OpenAiCodex, &codex_token(Some("acct-test")), codex_catalog()).await;
    let server = serve(200, json!({"models": []})).await;
    let token = codex_token(None);
    let models =
        AccountModelList::OpenAiCodex.personalize(codex_catalog(), &context(&server, Some(&token))).await.saved();
    assert_eq!(models, codex_catalog());
    assert!(server.requests().is_empty(), "a token without a ChatGPT account is not sent");
}

fn anthropic_catalog() -> Vec<Model> {
    let claude = |id| chat("anthropic", "anthropic-messages", id);
    vec![claude("claude-opus-4-8"), claude("claude-sonnet-5"), claude("claude-haiku-4-5"), claude("claude-fable-5")]
}

#[tokio::test]
async fn anthropic_follows_pages_and_sends_the_providers_auth_headers() {
    let server = RouteServer::start(|request| {
        let data = if request.path.contains("after_id=claude-sonnet-5") {
            json!({"data": [{"id": "claude-opus-5-5", "display_name": "Claude Opus 5.5", "max_tokens": 128_000}],
                   "has_more": false, "last_id": "claude-opus-5-5"})
        } else {
            json!({"data": [{"id": "claude-opus-4-8", "max_input_tokens": 1_000_000, "max_tokens": 64_000},
                            {"id": "claude-sonnet-5", "display_name": "Claude Sonnet 5"}],
                   "has_more": true, "last_id": "claude-sonnet-5"})
        };
        Reply::json(200, &data)
    })
    .await;
    let list = AccountModelList::Anthropic;
    let models = list.personalize(anthropic_catalog(), &context(&server, Some("sk-ant-api-test"))).await.saved();

    assert_eq!(ids(&models), ["claude-opus-4-8", "claude-sonnet-5", "claude-opus-5-5"]);
    assert_eq!((models[0].context_window, models[0].max_tokens), (1_000_000, 64_000));
    assert_eq!((models[2].name.as_str(), models[2].max_tokens), ("Claude Opus 5.5", 128_000));
    assert_eq!(models[2].compat, Some(json!({"flag": "claude-opus-4-8"})));
    let requests = server.requests();
    assert_eq!(requests[0].path, "/v1/models?limit=1000");
    assert_eq!(requests[1].path, "/v1/models?limit=1000&after_id=claude-sonnet-5");
    assert_eq!(requests[0].headers["x-api-key"], "sk-ant-api-test");
    assert_eq!(requests[0].headers["anthropic-version"], "2023-06-01");
    assert!(!requests[0].headers.contains_key("authorization"));

    list.personalize(anthropic_catalog(), &context(&server, Some("sk-ant-oat-test"))).await.saved();
    let oauth = &server.requests()[2];
    assert_eq!(oauth.headers["authorization"], "Bearer sk-ant-oat-test");
    assert_eq!(oauth.headers["anthropic-beta"], "oauth-2025-04-20");
    assert!(!oauth.headers.contains_key("x-api-key"));
}

#[tokio::test]
async fn anthropic_falls_back_to_the_catalog() {
    assert_fallbacks(AccountModelList::Anthropic, "sk-ant-api-test", anthropic_catalog()).await;
    let endless =
        RouteServer::start(|_| Reply::json(200, &json!({"data": [], "has_more": true, "last_id": "x"}))).await;
    let models =
        AccountModelList::Anthropic.personalize(anthropic_catalog(), &context(&endless, Some("k"))).await.saved();
    assert_eq!(models, anthropic_catalog(), "pages that never end change nothing");
}

fn copilot_catalog() -> Vec<Model> {
    vec![
        chat("github-copilot", "anthropic-messages", "claude-sonnet-4.6"),
        chat("github-copilot", "openai-responses", "gpt-5.4"),
        chat("github-copilot", "openai-completions", "gpt-4.1"),
        chat("github-copilot", "openai-responses", "gpt-5-mini"),
    ]
}

fn copilot_entry(id: &str, kind: &str, picker: bool, policy: &str) -> serde_json::Value {
    json!({"id": id, "name": id.to_uppercase(), "model_picker_enabled": picker, "policy": {"state": policy},
           "capabilities": {"type": kind, "supports": {"tool_calls": true},
                            "limits": {"max_context_window_tokens": 144_000, "max_output_tokens": 32_000}}})
}

#[tokio::test]
async fn copilot_lists_the_pickers_chat_models() {
    let mut no_tools = copilot_entry("gpt-5-mini", "chat", true, "enabled");
    no_tools["capabilities"]["supports"]["tool_calls"] = json!(false);
    let body = json!({"data": [
        copilot_entry("claude-sonnet-4.6", "chat", true, "enabled"),
        copilot_entry("gpt-5.4", "chat", true, "disabled"),
        copilot_entry("gpt-4.1", "chat", false, "enabled"),
        copilot_entry("claude-opus-4.8", "chat", true, "unconfigured"),
        copilot_entry("text-embedding-3-small", "embeddings", true, "enabled"),
        no_tools,
    ]});
    let server = serve(200, body).await;
    let list = AccountModelList::GitHubCopilot;
    let models = list.personalize(copilot_catalog(), &context(&server, Some("copilot-test-token"))).await.saved();

    assert_eq!(ids(&models), ["claude-sonnet-4.6", "claude-opus-4.8"]);
    assert_eq!((models[0].context_window, models[0].max_tokens), (144_000, 32_000));
    assert_eq!((models[1].name.as_str(), models[1].api.as_str()), ("CLAUDE-OPUS-4.8", "anthropic-messages"));
    let request = &server.requests()[0];
    assert_eq!(request.path, "/models");
    assert_eq!(request.headers["authorization"], "Bearer copilot-test-token");
    assert_eq!(request.headers["user-agent"], "GitHubCopilotChat/0.35.0");
    assert_eq!(request.headers["copilot-integration-id"], "vscode-chat");
    assert!(request.headers.contains_key("x-github-api-version"));
}

#[tokio::test]
async fn copilot_falls_back_to_the_catalog() {
    assert_fallbacks(AccountModelList::GitHubCopilot, "copilot-test-token", copilot_catalog()).await;
}

fn openai_catalog() -> Vec<Model> {
    let gpt = |id| chat("openai", "openai-responses", id);
    vec![gpt("gpt-5.4"), gpt("gpt-5.5"), gpt("o3"), classifier("openai", "judge")]
}

#[tokio::test]
async fn openai_and_xai_add_only_chat_models_of_catalog_families() {
    let body = json!({"object": "list", "data": [
        {"id": "gpt-5.4", "object": "model"}, {"id": "gpt-5.9"}, {"id": "text-embedding-3-large"},
        {"id": "gpt-image-2"}, {"id": "gpt-3.5-turbo-instruct"}, {"id": "gpt-4o-mini-search-preview"}, {"id": ""},
    ]});
    let server = serve(200, body).await;
    let models =
        AccountModelList::OpenAi.personalize(openai_catalog(), &context(&server, Some("sk-test"))).await.saved();
    assert_eq!(ids(&models), ["gpt-5.4", "judge", "gpt-5.9"]);
    assert_eq!(models[2].compat, Some(json!({"flag": "gpt-5.5"})), "copied from the closest catalog model");
    let request = &server.requests()[0];
    assert_eq!((request.path.as_str(), request.headers["authorization"].as_str()), ("/models", "Bearer sk-test"));

    let grok = |id| chat("xai", "openai-responses", id);
    let body = json!({"data": [{"id": "grok-4.5"}, {"id": "grok-code-fast-1"}, {"id": "grok-imagine-video"}]});
    let server = serve(200, body).await;
    let catalog = vec![grok("grok-4.3"), grok("grok-4.5")];
    let models = AccountModelList::Xai.personalize(catalog, &context(&server, Some("xai-test"))).await.saved();
    assert_eq!(ids(&models), ["grok-4.5", "grok-code-fast-1"]);
    assert_eq!(server.requests()[0].headers["authorization"], "Bearer xai-test");
}

#[tokio::test]
async fn openai_chatgpt_logins_take_the_accounts_default_context_size() {
    let body = json!({"models": [
        {"slug": "gpt-5.4", "context_window": 500_000, "max_context_window": 872_000},
        {"slug": "gpt-5.5", "max_context_window": 872_000}, {"slug": ""}, {"context_window": 1},
    ]});
    let server = serve(200, body).await;
    let models =
        AccountModelList::OpenAi.personalize(openai_catalog(), &context(&server, Some("oauth-token"))).await.saved();
    assert_eq!(ids(&models), ["gpt-5.4", "gpt-5.5", "judge"]);
    let windows: Vec<u64> = models.iter().map(|model| model.context_window).collect();
    assert_eq!(windows, [500_000, 100_000, 100_000]);
    assert_eq!(server.requests()[0].headers["authorization"], "Bearer oauth-token");
}

#[tokio::test]
async fn openai_and_xai_fall_back_to_the_catalog() {
    assert_fallbacks(AccountModelList::OpenAi, "sk-test", openai_catalog()).await;
    assert_fallbacks(AccountModelList::Xai, "xai-test", vec![chat("xai", "openai-responses", "grok-4.5")]).await;
}

fn google_catalog() -> Vec<Model> {
    let gemini = |id| chat("google", "google-generative-ai", id);
    vec![gemini("gemini-2.5-pro"), gemini("gemini-2.5-flash"), gemini("gemini-3.8-flash")]
}

#[tokio::test]
async fn google_follows_pages_and_keeps_generate_content_models() {
    let server = RouteServer::start(|request| {
        let body = if request.path.contains("pageToken=page-2") {
            json!({"models": [
                {"name": "models/gemini-3.9-flash", "displayName": "Gemini 3.9 Flash",
                 "supportedGenerationMethods": ["generateContent", "countTokens"]},
                {"name": "models/gemini-2.5-flash", "supportedGenerationMethods": ["bidiGenerateContent"]},
                {"name": "models/imagen-4.0-generate-001", "supportedGenerationMethods": ["predict"]},
            ]})
        } else {
            json!({"models": [
                {"name": "models/gemini-2.5-pro", "inputTokenLimit": 1_048_576, "outputTokenLimit": 65_536,
                 "supportedGenerationMethods": ["generateContent"]},
                {"name": "models/gemini-embedding-001", "supportedGenerationMethods": ["embedContent"]},
            ], "nextPageToken": "page-2"})
        };
        Reply::json(200, &body)
    })
    .await;
    let list = AccountModelList::Google;
    let models = list.personalize(google_catalog(), &context(&server, Some("gemini-test"))).await.saved();

    assert_eq!(ids(&models), ["gemini-2.5-pro", "gemini-3.9-flash"]);
    assert_eq!((models[0].context_window, models[0].max_tokens), (1_048_576, 65_536));
    assert_eq!(
        (models[1].name.as_str(), models[1].compat.clone()),
        ("Gemini 3.9 Flash", Some(json!({"flag": "gemini-3.8-flash"})))
    );
    let requests = server.requests();
    assert_eq!(requests[0].path, "/models?pageSize=1000");
    assert_eq!(requests[1].path, "/models?pageSize=1000&pageToken=page-2");
    assert_eq!(requests[0].headers["x-goog-api-key"], "gemini-test");
    assert!(!requests[0].headers.contains_key("authorization"));
}

#[tokio::test]
async fn google_falls_back_to_the_catalog() {
    assert_fallbacks(AccountModelList::Google, "gemini-test", google_catalog()).await;
}
