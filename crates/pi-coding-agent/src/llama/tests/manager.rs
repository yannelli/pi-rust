//! The `/llama` flow with a scripted UI against a local router and Hugging Face.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;

use crate::llama::view::ProgressState;
use crate::llama::*;
use crate::platform::test_http::{TestReply, TestServer, client};

#[derive(Default)]
struct ScriptedUi {
    actions: Mutex<VecDeque<LlamaManagerAction>>,
    selections: Mutex<VecDeque<Option<String>>>,
    confirms: Mutex<VecDeque<bool>>,
    search: Mutex<Option<String>>,
    /// Ask to stop on the first progress screen.
    stop_progress: bool,
    log: Mutex<Vec<String>>,
}

impl ScriptedUi {
    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    fn push(&self, entry: String) {
        self.log.lock().unwrap().push(entry);
    }
}

#[async_trait]
impl LlamaUi for ScriptedUi {
    async fn show_models(&self, _server_url: &str, models: &[LlamaModelInfo]) -> LlamaManagerAction {
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        self.push(format!("models {}", ids.join(",")));
        self.actions.lock().unwrap().pop_front().unwrap_or(LlamaManagerAction::Close)
    }

    async fn select(&self, title: &str, options: &[String]) -> Option<String> {
        self.push(format!("select {title} [{}]", options.join("|")));
        self.selections.lock().unwrap().pop_front().flatten()
    }

    async fn confirm(&self, title: &str, message: &str) -> bool {
        self.push(format!("confirm {title} {message}"));
        self.confirms.lock().unwrap().pop_front().unwrap_or(false)
    }

    async fn connection_error(&self, _server_url: &str, message: &str) -> ConnectionChoice {
        self.push(format!("connection {message}"));
        ConnectionChoice::Close
    }

    async fn search_models(&self, client: &HuggingFaceClient) -> Option<String> {
        let found = client.search("qwen", &tokio_util::sync::CancellationToken::new()).await.ok()?;
        self.push(format!("search {}", found.len()));
        self.search.lock().unwrap().clone()
    }

    fn show_status(&self, title: &str, message: &str) {
        self.push(format!("status {title} {message}"));
    }

    async fn progress(&self, _state: &ProgressState) {
        if self.stop_progress {
            tokio::time::sleep(Duration::from_millis(30)).await;
        } else {
            std::future::pending::<()>().await;
        }
    }

    fn update_progress(&self, _state: &ProgressState) {}

    fn notify(&self, message: &str, level: NotifyLevel) {
        self.push(format!("notify {level:?} {message}"));
    }
}

#[derive(Default)]
struct CountingSync(Mutex<usize>);

#[async_trait]
impl LlamaCatalogSync for CountingSync {
    async fn refresh_llama_models(&self) -> Result<(), String> {
        *self.0.lock().unwrap() += 1;
        Ok(())
    }
}

type Router = Arc<Mutex<Vec<(String, String)>>>;

/// A router whose loads and unloads take effect at once.
async fn router(models: &[(&str, &str)]) -> (TestServer, Router) {
    let state: Router =
        Arc::new(Mutex::new(models.iter().map(|(id, status)| (id.to_string(), status.to_string())).collect()));
    let shared = state.clone();
    let server = TestServer::start(move |request| {
        let mut models = shared.lock().unwrap();
        let target = request.json()["model"].as_str().unwrap_or_default().to_string();
        let set = |models: &mut Vec<(String, String)>, status: &str| {
            for model in models.iter_mut().filter(|model| model.0 == target) {
                model.1 = status.into();
            }
        };
        match (request.method.as_str(), request.path()) {
            ("POST", "/models/load") if target == "slow" => return TestReply::json(&json!({"success": true})),
            ("POST", "/models/load") => set(&mut models, "loaded"),
            ("POST", "/models/unload") => set(&mut models, "unloaded"),
            ("POST", "/models") => models.push((target.clone(), "unloaded".into())),
            (_, "/models/sse") => return TestReply::status(404, ""),
            (_, "/api/models") => return TestReply::json(&json!([{"id": "owner/repo", "downloads": 10}])),
            (_, "/api/models/owner/repo") => {
                return TestReply::json(&json!({"id": "owner/repo", "gated": "auto", "siblings": [
                    {"rfilename": "repo-Q8_0.gguf", "size": 8000}, {"rfilename": "repo-Q4_K_M.gguf", "size": 4096}]}));
            }
            _ => {}
        }
        let data: Vec<_> = models.iter().map(|(id, status)| json!({"id": id, "status": {"value": status}})).collect();
        TestReply::json(&json!({ "data": data }))
    })
    .await;
    (server, state)
}

async fn run(server: &TestServer, ui: &ScriptedUi) -> usize {
    let provider = LlamaProvider::new();
    let sync = CountingSync::default();
    let manager = LlamaManager {
        client: LlamaClient::new(&server.url, None).unwrap().with_http(client()),
        provider: &provider,
        ui,
        sync: &sync,
        hugging_face: HuggingFaceClient::new(None, Some(&server.url)).with_http(client()),
    };
    manager.run().await;
    *sync.0.lock().unwrap()
}

#[tokio::test]
async fn loading_can_unload_other_models_first() {
    let (server, state) = router(&[("a", "loaded"), ("b", "unloaded")]).await;
    let ui = ScriptedUi::default();
    ui.actions.lock().unwrap().push_back(LlamaManagerAction::Model(Box::new(LlamaModelInfo::new("b", "unloaded"))));
    ui.selections.lock().unwrap().push_back(Some("Unload all and load".into()));
    let syncs = run(&server, &ui).await;
    let log = ui.log();
    assert_eq!(log[0], "models a,b");
    assert_eq!(log[1], "select 1 model is loaded [Unload all and load|Keep loaded and load|Cancel]");
    assert!(log.contains(&"notify Info Loaded b".to_string()), "{log:?}");
    assert_eq!(*state.lock().unwrap(), [("a".to_string(), "unloaded".to_string()), ("b".into(), "loaded".into())]);
    assert!(syncs >= 3);
}

#[tokio::test]
async fn unloading_asks_first() {
    let (server, state) = router(&[("a", "sleeping")]).await;
    let ui = ScriptedUi::default();
    let action = LlamaManagerAction::Model(Box::new(LlamaModelInfo::new("a", "sleeping")));
    ui.actions.lock().unwrap().extend([action.clone(), action]);
    ui.confirms.lock().unwrap().extend([false, true]);
    run(&server, &ui).await;
    let log = ui.log();
    assert_eq!(log.iter().filter(|entry| *entry == "confirm Unload model? a").count(), 2);
    assert!(log.contains(&"notify Info Unloaded a".to_string()));
    assert_eq!(state.lock().unwrap()[0].1, "unloaded");
}

#[tokio::test]
async fn downloads_warn_about_gated_repositories_and_offer_quantizations() {
    let (server, state) = router(&[]).await;
    let ui = ScriptedUi::default();
    ui.actions.lock().unwrap().push_back(LlamaManagerAction::Download);
    *ui.search.lock().unwrap() = Some("owner/repo".into());
    ui.selections.lock().unwrap().extend([Some("Continue".into()), Some("Q4_K_M · 4.00 KiB · recommended".into())]);
    run(&server, &ui).await;
    let log = ui.log();
    assert!(log.contains(&"search 1".to_string()));
    assert!(log.contains(&"status Loading model details owner/repo".to_string()));
    let gated = log.iter().find(|entry| entry.starts_with("select Hugging Face access required")).unwrap();
    assert!(gated.contains("Accept the access terms at:\nhttps://huggingface.co/owner/repo"), "{gated}");
    assert!(log.contains(
        &"select Select quantization\nowner/repo [Q4_K_M · 4.00 KiB · recommended|Q8_0 · 7.81 KiB]".to_string()
    ));
    assert!(log.contains(&"notify Info Downloaded owner/repo:Q4_K_M".to_string()), "{log:?}");
    assert_eq!(state.lock().unwrap()[0].0, "owner/repo:Q4_K_M");
}

#[tokio::test]
async fn stopping_a_load_unloads_and_restores_replaced_models() {
    let (server, state) = router(&[("a", "loaded"), ("slow", "unloaded")]).await;
    let ui = ScriptedUi { stop_progress: true, ..ScriptedUi::default() };
    ui.actions.lock().unwrap().push_back(LlamaManagerAction::Model(Box::new(LlamaModelInfo::new("slow", "unloaded"))));
    ui.selections.lock().unwrap().push_back(Some("Unload all and load".into()));
    ui.confirms.lock().unwrap().extend([false, true]);
    run(&server, &ui).await;
    let log = ui.log();
    assert_eq!(log.iter().filter(|entry| *entry == "confirm Stop loading? slow").count(), 2);
    assert!(log.contains(&"notify Info Restoring previously loaded models".to_string()), "{log:?}");
    assert!(
        server.requests().iter().any(|request| request.path() == "/models/unload" && request.json()["model"] == "slow")
    );
    assert_eq!(state.lock().unwrap()[0], ("a".to_string(), "loaded".to_string()));
}

#[tokio::test]
async fn unreachable_routers_offer_retry() {
    let ui = ScriptedUi::default();
    let provider = LlamaProvider::new();
    let sync = CountingSync::default();
    let manager = LlamaManager {
        client: LlamaClient::new("http://127.0.0.1:9", None).unwrap().with_http(client()),
        provider: &provider,
        ui: &ui,
        sync: &sync,
        hugging_face: HuggingFaceClient::new(None, None),
    };
    manager.run().await;
    assert_eq!(ui.log(), ["connection Could not connect to the server."]);
    assert_eq!(configured_llama_client(None).err().as_deref(), Some("Configure llama.cpp with /login llama.cpp"));
}
