//! `/llama` session methods and the llama.cpp provider in the model registry, against a local
//! router (upstream `test/llama-extension.test.ts`).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use pi_ai::auth::{ApiKeyCredential, Credential};
use pi_coding_agent::agent_session::auth::request_auth;
use pi_coding_agent::llama::{LlamaError, LlamaProgress};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::harness::{Options, harness};
use crate::http_mock::{Reply, Server};

type Models = Arc<Mutex<Vec<(String, String)>>>;

/// A router whose loads, unloads, and downloads take effect at once.
async fn router(models: &[(&str, &str)]) -> (Server, Models) {
    let state: Models =
        Arc::new(Mutex::new(models.iter().map(|(id, status)| (id.to_string(), status.to_string())).collect()));
    let shared = state.clone();
    let server = Server::start(move |request| {
        let mut models = shared.lock().unwrap();
        let target = request.json()["model"].as_str().unwrap_or_default().to_string();
        let mut set = |status: &str| {
            for model in models.iter_mut().filter(|model| model.0 == target) {
                model.1 = status.into();
            }
        };
        match (request.method.as_str(), request.path()) {
            ("POST", "/models/load") => set("loaded"),
            ("POST", "/models/unload") => set("unloaded"),
            ("POST", "/models") => models.push((target.clone(), "unloaded".into())),
            (_, "/models/sse") => return Reply::status(404),
            (_, "/props") => return Reply::json(&json!({"chat_template": "{% if enable_thinking %}{% endif %}"})),
            _ => {}
        }
        let data: Vec<_> = models.iter().map(|(id, status)| json!({"id": id, "status": {"value": status}})).collect();
        Reply::json(&json!({ "data": data }))
    })
    .await;
    (server, state)
}

fn credential(url: &str) -> BTreeMap<String, Credential> {
    let env = BTreeMap::from([("LLAMA_BASE_URL".to_string(), url.to_string())]);
    BTreeMap::from([("llama.cpp".to_string(), Credential::ApiKey(ApiKeyCredential { key: None, env: Some(env) }))])
}

fn llama_ids(h: &crate::harness::Harness) -> Vec<String> {
    let models = h.session.model_runtime().available_models();
    models.into_iter().filter(|model| model.provider == "llama.cpp").map(|model| model.id).collect()
}

#[tokio::test]
async fn router_models_appear_in_the_registry_and_follow_loads_and_unloads() {
    let (server, state) = router(&[("a", "loaded"), ("b", "unloaded"), ("c", "sleeping")]).await;
    let h = harness(Options { credentials: credential(&server.url), ..Options::default() }).await;
    assert!(h.session.llama_provider().is_some());
    assert!(llama_ids(&h).is_empty(), "startup restores the store only");

    let catalog = h.session.llama_list_models().await.unwrap();
    assert_eq!(catalog.len(), 3);
    assert_eq!(llama_ids(&h), ["a", "c"], "loaded and sleeping models are selectable");
    let model = h.session.model_runtime().get_model("llama.cpp", "a").unwrap();
    assert_eq!((model.api.as_str(), model.reasoning), ("openai-completions", true));
    let auth = request_auth(h.session.model_runtime(), &model).await.unwrap();
    assert_eq!(auth.model.base_url, format!("{}/v1", server.url));
    assert_eq!(auth.auth.api_key.as_deref(), Some("local"));
    let stored = std::fs::read_to_string(h.agent_dir.join("models-store.json")).unwrap();
    assert!(stored.contains("\"llama.cpp\""), "the router catalog is cached for offline starts");

    let reports: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = reports.clone();
    let progress = move |update: LlamaProgress| sink.lock().unwrap().push(update.message);
    let loaded = h.session.llama_load_model("b", &progress, &CancellationToken::new()).await.unwrap();
    assert_eq!(loaded.id, "b");
    assert!(reports.lock().unwrap().contains(&"Loading model".to_string()));
    assert_eq!(llama_ids(&h), ["a", "b", "c"]);

    h.session.llama_unload_model("a", &CancellationToken::new()).await.unwrap();
    assert_eq!(llama_ids(&h), ["b", "c"]);

    let downloaded = h.session.llama_download_model("owner/repo:Q4_K_M", &|_| {}, &CancellationToken::new()).await;
    assert!(downloaded.unwrap().iter().any(|model| model.id == "owner/repo:Q4_K_M"));
    assert_eq!(state.lock().unwrap().len(), 4);

    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = h.session.llama_load_model("a", &|_| {}, &cancel).await.unwrap_err();
    assert_eq!(error, LlamaError::Cancelled);
}

#[tokio::test]
async fn sessions_restore_cached_router_models_and_report_missing_configuration() {
    let (server, _) = router(&[("a", "loaded")]).await;
    let h = harness(Options { credentials: credential(&server.url), ..Options::default() }).await;
    h.session.llama_list_models().await.unwrap();
    let services = pi_coding_agent::agent_session::create_agent_session_services(
        pi_coding_agent::agent_session::CreateServicesOptions {
            settings: Some(h.settings.clone()),
            credentials: Some(Arc::new(pi_ai::auth::InMemoryCredentialStore::new(credential(&server.url)))),
            ..pi_coding_agent::agent_session::CreateServicesOptions::new(&h.cwd, &h.agent_dir)
        },
    )
    .await;
    let restored = services.model_runtime.available_models();
    assert!(restored.iter().any(|model| model.provider == "llama.cpp" && model.id == "a"));

    let unconfigured = harness(Options::default()).await;
    let notice = unconfigured.session.llama_client().await.err();
    assert_eq!(notice.as_deref(), Some("Configure llama.cpp with /login llama.cpp"));
}

#[tokio::test]
async fn builtin_toggle_removes_the_provider() {
    let (server, _) = router(&[("a", "loaded")]).await;
    let h = harness(Options {
        credentials: credential(&server.url),
        settings: json!({"extensions": ["-builtin:llama.cpp"]}),
        ..Options::default()
    })
    .await;
    assert!(h.session.llama_provider().is_none());
    assert!(h.session.llama_client().await.is_err());
    assert!(h.session.llama_list_models().await.is_err());
    assert!(server.requests().is_empty());
}
