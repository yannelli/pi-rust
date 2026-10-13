//! Port of upstream `llama-extension.test.ts`: provider catalog, auth, and display helpers.

mod client;
mod manager;

use std::collections::BTreeMap;

use pi_ai::auth::{ApiKeyCredential, AuthType, Credential};
use serde_json::{Value, json};

use super::view::*;
use super::*;
use crate::catalog::tests::{RecordingPublisher, ids, refresh_with};
use crate::platform::test_http::{TestReply, TestServer, client};

pub(super) fn info(value: Value) -> LlamaModelInfo {
    LlamaModelInfo::from_value(&value).unwrap()
}

fn credential(url: &str) -> Credential {
    let env = BTreeMap::from([(ENV_LLAMA_BASE_URL.to_string(), url.to_string())]);
    Credential::ApiKey(ApiKeyCredential { key: Some("local".into()), env: Some(env) })
}

fn provider() -> LlamaProvider {
    LlamaProvider::new().with_http(client())
}

fn classifiers(provider: &LlamaProvider) -> Vec<(String, String)> {
    let all = provider.all_models();
    all.into_iter().filter(|model| model.model_type.as_deref() == Some("classifier")).map(|m| (m.id, m.api)).collect()
}

#[test]
fn normalizes_management_and_inference_urls() {
    assert_eq!(normalize_llama_server_url("http://127.0.0.1:8080/v1/").unwrap(), "http://127.0.0.1:8080");
    assert_eq!(
        normalize_llama_server_url(" https://example.com/prefix/v1?x=1#y ").unwrap(),
        "https://example.com/prefix"
    );
    assert!(normalize_llama_server_url("file:///tmp/llama").unwrap_err().contains("http or https"));
    assert_eq!(llama_inference_url("http://h:1/").unwrap(), "http://h:1/v1");
}

#[test]
fn exposes_loaded_and_sleeping_models_with_router_metadata() {
    let provider = provider();
    let catalog = [
        info(json!({"id": "loaded", "status": {"value": "loaded", "args": ["llama-server", "--n-gpu-layers", "999"]},
            "architecture": {"input_modalities": ["text", "image"]}, "meta": {"n_ctx": 65536, "n_ctx_train": 131072}})),
        info(json!({"id": "sleeping", "status": {"value": "sleeping"}})),
        info(json!({"id": "unloaded", "status": {"value": "unloaded"}})),
        info(json!({"id": "loading", "status": {"value": "loading"}})),
    ];
    provider.set_catalog(&catalog, "http://localhost:8080", false);
    let models = provider.models();
    assert_eq!(ids(&models), ["loaded", "sleeping"]);
    assert_eq!(models[0].base_url, "http://localhost:8080/v1");
    assert_eq!((models[0].context_window, models[0].max_tokens), (65536, 65536));
    assert_eq!(models[0].input, ["text", "image"]);
    assert_eq!(models[1].context_window, 128_000);
    let compat = models[0].compat.as_ref().unwrap();
    assert_eq!(compat["maxTokensField"], "max_tokens");
    assert_eq!(compat["supportsUsageInStreaming"], true);
    assert!(!models[0].reasoning);
}

#[tokio::test]
async fn discovers_chat_template_thinking_support_for_loaded_models() {
    let server = TestServer::start(|request| match request.path() {
        "/models" => {
            TestReply::json(&json!({"data": [{"id": "qwen", "status": {"value": "loaded"}, "meta": {"n_ctx": 32768}}]}))
        }
        "/props" => {
            assert_eq!(request.query("model").as_deref(), Some("qwen"));
            assert_eq!(request.query("autoload").as_deref(), Some("false"));
            TestReply::json(&json!({"chat_template": "{% if enable_thinking %}think{% endif %}"}))
        }
        _ => TestReply::status(404, ""),
    })
    .await;
    let provider = provider();
    let publisher = RecordingPublisher::new(None);
    refresh_with(&provider, &publisher, Some(&credential(&server.url)), true, false).await.unwrap();
    assert_eq!(server.count("/props"), 1);
    let model = &provider.models()[0];
    assert!(model.reasoning);
    let map = model.thinking_level_map.as_ref().unwrap();
    assert_eq!(map["off"].as_deref(), Some("off"));
    assert_eq!(map["medium"].as_deref(), Some("medium"));
    assert_eq!(map["high"], None);
    assert_eq!(model.compat.as_ref().unwrap()["thinkingFormat"], "qwen-chat-template");
}

#[tokio::test]
async fn persists_and_restores_selectable_models_for_cache_only_refreshes() {
    let server = TestServer::start(|request| match request.target.as_str() {
        "/models" => TestReply::json(&json!({"data": [
            {"id": "loaded", "status": {"value": "loaded"}, "meta": {"n_ctx": 32768}},
            {"id": "sleeping", "status": {"value": "sleeping"}, "meta": {"n_ctx": 32768}},
            {"id": "unloaded", "status": {"value": "unloaded"}},
        ]})),
        "/props?model=loaded&autoload=false" => TestReply::json(&json!({})),
        _ => TestReply::status(404, ""),
    })
    .await;
    let publisher = RecordingPublisher::new(None);
    let first = provider();
    refresh_with(&first, &publisher, Some(&credential(&server.url)), true, false).await.unwrap();
    assert_eq!(ids(&first.models()), ["loaded", "sleeping"]);
    let stored: Vec<(String, String)> =
        publisher.stored().unwrap().models.into_iter().map(|model| (model.id, model.api)).collect();
    let expected = [
        ("loaded", "openai-completions"),
        ("sleeping", "openai-completions"),
        ("loaded", "llama-cpp-classify"),
        ("sleeping", "llama-cpp-classify"),
    ];
    assert_eq!(stored, expected.map(|(id, api)| (id.to_string(), api.to_string())));

    let second = provider();
    refresh_with(&second, &publisher, Some(&credential(&server.url)), false, false).await.unwrap();
    let models = second.models();
    assert_eq!(ids(&models), ["loaded", "sleeping"]);
    assert_eq!(models[0].base_url, format!("{}/v1", server.url));
    assert_eq!(models[1].context_window, 32768);
    let classifier = second.all_models().into_iter().find(|model| model.api == "llama-cpp-classify").unwrap();
    assert_eq!((classifier.base_url.as_str(), classifier.context_window), (server.url.as_str(), 32768));
}

#[tokio::test]
async fn keeps_cached_context_for_unloaded_autoload_presets() {
    let state = std::sync::Arc::new(std::sync::Mutex::new((true, None::<Vec<&'static str>>)));
    let shared = state.clone();
    let server = TestServer::start(move |request| {
        let (loaded, args) = shared.lock().unwrap().clone();
        match request.target.as_str() {
            "/models" if loaded => TestReply::json(&json!({"data": [{"id": "qwen", "status": {"value": "loaded"},
                "source": "preset", "meta": {"n_ctx": 65536, "n_ctx_train": 128000}}]})),
            "/models" => {
                let status = match args {
                    Some(args) => json!({"value": "unloaded", "args": args}),
                    None => json!({"value": "unloaded"}),
                };
                TestReply::json(&json!({"data": [{"id": "qwen", "status": status, "source": "preset",
                    "meta": {"n_ctx_train": 128000}}]}))
            }
            "/props?model=qwen&autoload=false" => TestReply::json(&json!({})),
            "/props" => TestReply::json(&json!({"role": "router", "models_autoload": true})),
            _ => TestReply::status(404, ""),
        }
    })
    .await;
    let publisher = RecordingPublisher::new(None);
    let windows = || publisher.stored().unwrap().models.iter().map(|model| model.context_window).collect::<Vec<_>>();
    refresh_with(&provider(), &publisher, Some(&credential(&server.url)), true, false).await.unwrap();
    assert_eq!(windows(), [65536, 65536]);

    state.lock().unwrap().0 = false;
    let second = provider();
    refresh_with(&second, &publisher, Some(&credential(&server.url)), true, false).await.unwrap();
    assert_eq!(second.models()[0].context_window, 65536);
    assert_eq!(windows(), [65536, 65536]);

    state.lock().unwrap().1 = Some(vec!["llama-server", "--ctx-size", "32768"]);
    refresh_with(&second, &publisher, Some(&credential(&server.url)), true, false).await.unwrap();
    assert_eq!(windows(), [32768, 32768]);
}

#[tokio::test]
async fn unloaded_presets_appear_only_with_router_autoload() {
    let autoload = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let flag = autoload.clone();
    let server = TestServer::start(move |request| {
        assert_eq!(request.header("authorization"), Some("Bearer local"));
        match request.target.as_str() {
            "/models" => TestReply::json(&json!({"data": [
                {"id": "preset", "status": {"value": "unloaded"}, "source": "preset", "meta": {"n_ctx": 65536}},
                {"id": "failed-preset", "status": {"value": "unloaded", "failed": true}, "source": "preset"},
                {"id": "cache", "status": {"value": "unloaded"}, "source": "cache"},
                {"id": "models-dir", "status": {"value": "unloaded"}, "source": "models_dir"},
            ]})),
            "/props" => TestReply::json(&json!({"models_autoload": flag.load(std::sync::atomic::Ordering::SeqCst)})),
            _ => TestReply::status(404, ""),
        }
    })
    .await;
    let publisher = RecordingPublisher::new(None);
    let enabled = provider();
    refresh_with(&enabled, &publisher, Some(&credential(&server.url)), true, false).await.unwrap();
    assert_eq!(server.count("/props"), 1);
    assert_eq!(ids(&enabled.models()), ["preset"]);
    assert_eq!(classifiers(&enabled), [("preset".to_string(), "llama-cpp-classify".to_string())]);

    autoload.store(false, std::sync::atomic::Ordering::SeqCst);
    let disabled = provider();
    refresh_with(&disabled, &RecordingPublisher::new(None), Some(&credential(&server.url)), true, false).await.unwrap();
    assert!(disabled.models().is_empty());
}

#[test]
fn decision_models_are_classifiers_and_hybrids_also_chat() {
    let provider = provider();
    let catalog = [
        info(
            json!({"id": "decide", "status": {"value": "sleeping"}, "architecture": {"output_modalities": ["decisions"]}}),
        ),
        info(
            json!({"id": "hybrid", "status": {"value": "loaded"}, "architecture": {"output_modalities": ["text", "decisions"]}}),
        ),
    ];
    provider.set_catalog(&catalog, "http://localhost:8080", false);
    assert_eq!(ids(&provider.models()), ["hybrid"]);
    let expected = [("decide", "typesafe-system-one"), ("hybrid", "typesafe-system-one")];
    assert_eq!(classifiers(&provider), expected.map(|(id, api)| (id.to_string(), api.to_string())));
    let decide = provider.all_models().into_iter().find(|model| model.id == "decide").unwrap();
    assert_eq!(decide.base_url, "http://localhost:8080/v1");
}

#[test]
fn auth_stays_dormant_until_configured() {
    let empty = |_: &str| None;
    assert_eq!(check_llama_auth(None, &empty).unwrap(), None);
    assert_eq!(resolve_llama_auth(None, &empty).unwrap(), None);
    let env = |name: &str| match name {
        "LLAMA_BASE_URL" => Some("http://10.0.0.2:8080/v1".to_string()),
        "LLAMA_API_KEY" => Some("from-env".to_string()),
        _ => None,
    };
    let check = check_llama_auth(None, &env).unwrap().unwrap();
    assert_eq!((check.auth_type, check.source.as_str()), (AuthType::ApiKey, "LLAMA_BASE_URL"));
    let resolved = resolve_llama_auth(None, &env).unwrap().unwrap();
    assert_eq!(resolved.auth.api_key.as_deref(), Some("from-env"));
    assert_eq!(resolved.auth.base_url.as_deref(), Some("http://10.0.0.2:8080/v1"));
    assert_eq!(resolved.env.unwrap()["LLAMA_BASE_URL"], "http://10.0.0.2:8080");
    let local = |name: &str| (name == "LLAMA_BASE_URL").then(|| "http://h:1".to_string());
    assert_eq!(resolve_llama_auth(None, &local).unwrap().unwrap().auth.api_key.as_deref(), Some("local"));
}

#[test]
fn display_helpers_match_the_upstream_view() {
    let loaded = info(json!({"id": "b", "status": {"value": "loaded"}, "meta": {"n_ctx": 65536}}));
    let args = info(json!({"id": "c", "status": {"value": "sleeping", "args": ["-c", "800"]}}));
    let downloading = info(json!({"id": "a", "status": {"value": "downloading"}}));
    let unloaded = info(json!({"id": "d", "status": {"value": "unloaded"}, "meta": {"n_ctx_train": 4096}}));
    assert_eq!(model_description(&loaded), "loaded · 66k context");
    assert_eq!(model_description(&args), "loaded · 800 context");
    assert_eq!(model_description(&downloading), "downloading");
    assert_eq!(model_description(&unloaded), "");
    let sorted = sort_models_for_display(&[unloaded.clone(), downloading, args, loaded]);
    assert_eq!(sorted.iter().map(|model| model.id.as_str()).collect::<Vec<_>>(), ["b", "a", "c", "d"]);
    assert_eq!(compact_count(999), "999");
    assert_eq!(compact_count(1_250), "1.3k");
    assert_eq!(compact_count(123_456), "123k");
    assert_eq!(compact_count(3_400_000), "3.4M");
    assert_eq!(compact_count(45_000_000), "45M");
    assert!(is_exact_model_query("owner/repo:Q4_K_M") && is_exact_model_query("owner/repo"));
    assert!(!is_exact_model_query("qwen coder") && !is_exact_model_query("owner/repo:a:b"));
    assert_eq!(parse_hugging_face_model("o/r:Q8_0"), ("o/r".to_string(), Some("Q8_0".to_string())));
    assert_eq!(parse_hugging_face_model("o/r"), ("o/r".to_string(), None));
    assert_eq!(progress_bar(0.5), format!("{}{} 50%", "█".repeat(20), "─".repeat(20)));
    assert_eq!(search_status("q", None), SEARCH_MIN_CHARS_STATUS);
    assert_eq!(search_status("qwen", Some(0)), NO_RESULTS_STATUS);
    assert_eq!(format_bytes(512.0), "512 B");
    assert_eq!(format_bytes(1024.0), "1.00 KiB");
    assert_eq!(format_bytes(5.5 * 1024.0 * 1024.0 * 1024.0 * 4.0), "22.0 GiB");
}
