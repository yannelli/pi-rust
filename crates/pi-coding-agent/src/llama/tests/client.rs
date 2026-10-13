//! Router client, SSE progress, Hugging Face, and `/login llama.cpp` against local servers.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use pi_ai::auth::{AuthEvent, AuthInteraction, AuthPrompt, AuthResult};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::llama::*;
use crate::platform::test_http::{TestReply, TestServer, client};

type Streams = Arc<Mutex<Vec<mpsc::UnboundedSender<Vec<u8>>>>>;

fn send(streams: &Streams, event: Value) {
    let frame = format!("data: {event}\r\n\r\n").into_bytes();
    for stream in streams.lock().unwrap().iter() {
        let _ = stream.send(frame.clone());
    }
}

fn sse(streams: &Streams) -> TestReply {
    let (sender, receiver) = mpsc::unbounded_channel();
    streams.lock().unwrap().push(sender);
    TestReply::Stream(receiver)
}

fn llama(server: &TestServer) -> LlamaClient {
    LlamaClient::new(&server.url, None).unwrap().with_http(client())
}

#[tokio::test]
async fn loads_with_sse_progress_and_waits_for_the_loaded_state() {
    let streams: Streams = Arc::default();
    let status = Arc::new(Mutex::new("unloaded"));
    let (events, state) = (streams.clone(), status.clone());
    let server = TestServer::start(move |request| match (request.method.as_str(), request.path()) {
        (_, "/models/sse") => sse(&events),
        ("POST", "/models/load") => {
            assert_eq!(request.json(), json!({"model": "test-model"}));
            *state.lock().unwrap() = "loading";
            let (events, state) = (events.clone(), state.clone());
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                let progress = json!({"stages": ["text_model", "mmproj_model"], "current": "text_model", "value": 0.5});
                send(
                    &events,
                    json!({"model": "test-model", "event": "status_change",
                    "data": {"status": "loading", "progress": progress}}),
                );
                *state.lock().unwrap() = "loaded";
                send(&events, json!({"model": "test-model", "event": "status_change", "data": {"status": "loaded"}}));
            });
            TestReply::json(&json!({"success": true}))
        }
        (_, "/models") => {
            TestReply::json(&json!({"data": [{"id": "test-model", "status": {"value": *state.lock().unwrap()}}]}))
        }
        _ => TestReply::status(404, ""),
    })
    .await;
    let progress = Mutex::new(Vec::<LlamaProgress>::new());
    let record = |entry: LlamaProgress| progress.lock().unwrap().push(entry);
    let model = llama(&server).load_and_wait("test-model", &record, &CancellationToken::new()).await.unwrap();
    assert!(model.is_status("loaded"));
    let progress = progress.into_inner().unwrap();
    let loading = progress.iter().find(|entry| entry.message == "Loading text model").unwrap();
    assert_eq!(loading.ratio, Some(0.25));
    assert_eq!(progress[0].message, "Loading model");
}

#[tokio::test]
async fn load_failures_report_the_exit_code() {
    let server = TestServer::start(|request| match request.path() {
        "/models/sse" => TestReply::status(404, ""),
        "/models/load" => TestReply::json(&json!({"success": true})),
        _ => TestReply::json(
            &json!({"data": [{"id": "m", "status": {"value": "unloaded", "failed": true, "exit_code": 3}}]}),
        ),
    })
    .await;
    let error = llama(&server).load_and_wait("m", &|_| {}, &CancellationToken::new()).await.unwrap_err();
    assert_eq!(error, LlamaError::Server("Model exited with code 3".into()));
}

#[tokio::test]
async fn downloads_with_byte_progress_and_returns_the_reloaded_catalog() {
    let streams: Streams = Arc::default();
    let status = Arc::new(Mutex::new("missing"));
    let (events, state) = (streams.clone(), status.clone());
    let server = TestServer::start(move |request| match (request.method.as_str(), request.path()) {
        (_, "/models/sse") => sse(&events),
        ("POST", "/models") => {
            *state.lock().unwrap() = "downloading";
            let (events, state) = (events.clone(), state.clone());
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                let files = json!({"https://example/model.gguf": {"done": 512, "total": 1024}});
                send(
                    &events,
                    json!({"model": "owner/repo:Q4_K_M", "event": "download_progress", "data": {"progress": files}}),
                );
                *state.lock().unwrap() = "unloaded";
                send(&events, json!({"model": "owner/repo:Q4_K_M", "event": "download_finished", "data": {}}));
            });
            TestReply::json(&json!({"success": true}))
        }
        (_, "/models") => {
            let current = *state.lock().unwrap();
            let data = if current == "missing" {
                json!([])
            } else {
                json!([{"id": "owner/repo:Q4_K_M", "status": {"value": current}}])
            };
            TestReply::json(&json!({ "data": data }))
        }
        _ => TestReply::status(404, ""),
    })
    .await;
    let progress = Mutex::new(Vec::<LlamaProgress>::new());
    let record = |entry: LlamaProgress| progress.lock().unwrap().push(entry);
    let models =
        llama(&server).download_and_wait("owner/repo:Q4_K_M", &record, &CancellationToken::new()).await.unwrap();
    assert_eq!(models, [LlamaModelInfo::new("owner/repo:Q4_K_M", "unloaded")]);
    let expected = LlamaProgress {
        message: "Downloading model".into(),
        ratio: Some(0.5),
        detail: Some("512 B / 1.00 KiB".into()),
    };
    assert!(progress.lock().unwrap().contains(&expected));
    assert!(server.requests().iter().any(|request| request.target == "/models?reload=1"));
}

#[tokio::test]
async fn download_failures_and_cancellation_end_the_wait() {
    let streams: Streams = Arc::default();
    let events = streams.clone();
    let server = TestServer::start(move |request| match (request.method.as_str(), request.path()) {
        (_, "/models/sse") => sse(&events),
        ("POST", "/models") => {
            let events = events.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                send(&events, json!({"model": "bad/repo", "event": "download_failed", "data": {"error": {"message": "401 gated"}}}));
            });
            TestReply::json(&json!({"success": true}))
        }
        _ => TestReply::json(&json!({"data": [{"id": "bad/repo", "status": {"value": "downloading"}}]})),
    })
    .await;
    let error = llama(&server).download_and_wait("bad/repo", &|_| {}, &CancellationToken::new()).await.unwrap_err();
    assert_eq!(error.to_string(), "401 gated");

    let cancel = CancellationToken::new();
    let stopper = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        stopper.cancel();
    });
    let error = llama(&server).load_and_wait("other", &|_| {}, &cancel).await.unwrap_err();
    assert!(matches!(error, LlamaError::Cancelled | LlamaError::Server(_)), "{error:?}");
}

#[tokio::test]
async fn list_rejects_non_router_servers_and_reports_errors() {
    let server = TestServer::start(|request| match request.target.as_str() {
        "/models" => TestReply::json(&json!({"data": [{"id": "single"}]})),
        "/models?reload=1" => TestReply::json(&json!({"object": "list"})),
        _ => TestReply::json(&json!({"error": {"message": "model not found"}})).header("x", "y"),
    })
    .await;
    let cancel = CancellationToken::new();
    let client = llama(&server);
    let error = client.list(false, &cancel).await.unwrap_err();
    assert_eq!(error.to_string(), "Server is not running in llama.cpp router mode");
    assert_eq!(
        client.list(true, &cancel).await.unwrap_err().to_string(),
        "llama.cpp returned an invalid model catalog"
    );
    let missing =
        TestServer::start(|_| TestReply::status(400, "{\"error\": {\"message\": \"model not found\"}}")).await;
    assert_eq!(llama(&missing).unload("x", &cancel).await.unwrap_err().to_string(), "model not found");
    let refused = LlamaClient::new("http://127.0.0.1:9", None).unwrap().with_http(client.http.clone());
    assert!(refused.list(false, &cancel).await.unwrap_err().is_connection_error());
}

#[tokio::test]
async fn searches_hugging_face_and_reads_quantizations_and_access() {
    let server = TestServer::start(|request| {
        assert_eq!(request.header("authorization"), Some("Bearer hf-secret"));
        match request.path() {
            "/api/models" => {
                assert_eq!(request.query("search").as_deref(), Some("qwen coder"));
                assert_eq!(request.query("filter").as_deref(), Some("gguf"));
                assert_eq!(request.query("sort").as_deref(), Some("downloads"));
                assert_eq!(request.query("limit").as_deref(), Some("20"));
                TestReply::json(&json!([{"id": "owner/model-GGUF", "downloads": 1200}, {"downloads": 3}]))
            }
            "/api/models/owner/model-GGUF" => {
                assert_eq!(request.query("blobs").as_deref(), Some("true"));
                TestReply::json(&json!({"id": "owner/model-GGUF", "gated": "manual", "siblings": [
                    {"rfilename": "model-Q5_K_M.gguf", "size": 6000},
                    {"rfilename": "model-Q4_K_M-00001-of-00002.gguf", "size": 2000},
                    {"rfilename": "model-Q4_K_M-00002-of-00002.gguf", "size": 3000},
                    {"rfilename": "sub/model.IQ2_XS.gguf"},
                    {"rfilename": "mmproj-F16.gguf", "size": 1000},
                    {"rfilename": "README.md", "size": 10},
                ]}))
            }
            _ => TestReply::status(429, "").header("ratelimit", "\"api\";r=0;t=42"),
        }
    })
    .await;
    let hf = HuggingFaceClient::new(Some("hf-secret".into()), Some(&server.url)).with_http(client());
    let cancel = CancellationToken::new();
    let results = hf.search("qwen coder", &cancel).await.unwrap();
    assert_eq!(results, [HuggingFaceModel { id: "owner/model-GGUF".into(), downloads: 1200 }]);
    let details = hf.details("owner/model-GGUF", &cancel).await.unwrap();
    assert_eq!(details.gated, GatedAccess::Manual);
    let quantizations: Vec<(&str, Option<u64>)> =
        details.quantizations.iter().map(|entry| (entry.name.as_str(), entry.size)).collect();
    assert_eq!(quantizations, [("Q4_K_M", Some(5000)), ("Q5_K_M", Some(6000)), ("IQ2_XS", None)]);
    let error = hf.details("other/repo", &cancel).await.unwrap_err();
    assert_eq!(error.to_string(), "Hugging Face rate limit reached; retry in 42s");
}

#[test]
fn hugging_face_tokens_come_from_the_environment_then_token_files() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let none = |_: &str| None;
    assert_eq!(
        find_hugging_face_token_with(&|name| (name == "HF_TOKEN").then(|| " hf-secret ".into()), &home).as_deref(),
        Some("hf-secret")
    );
    assert_eq!(find_hugging_face_token_with(&none, &home), None);
    std::fs::create_dir_all(home.join(".cache/huggingface")).unwrap();
    std::fs::write(home.join(".cache/huggingface/token"), "home-token\n").unwrap();
    assert_eq!(find_hugging_face_token_with(&none, &home).as_deref(), Some("home-token"));
    let hf_home = dir.path().join("hf");
    std::fs::create_dir_all(&hf_home).unwrap();
    std::fs::write(hf_home.join("token"), "hf-home-token").unwrap();
    let path = dir.path().join("explicit");
    std::fs::write(&path, "  ").unwrap();
    let (hf_home, path) = (hf_home.display().to_string(), path.display().to_string());
    let env = |name: &str| match name {
        "HF_TOKEN" => Some("  ".to_string()),
        "HF_TOKEN_PATH" => Some(path.clone()),
        "HF_HOME" => Some(hf_home.clone()),
        _ => None,
    };
    assert_eq!(find_hugging_face_token_with(&env, &home).as_deref(), Some("hf-home-token"));
}

struct Answers(Mutex<Vec<String>>, Mutex<Vec<AuthPrompt>>);

#[async_trait]
impl AuthInteraction for Answers {
    async fn prompt(&self, prompt: AuthPrompt, _cancel: CancellationToken) -> AuthResult<String> {
        self.1.lock().unwrap().push(prompt);
        Ok(self.0.lock().unwrap().remove(0))
    }

    fn notify(&self, _event: AuthEvent) {}
}

#[tokio::test]
async fn login_stores_the_url_and_optional_key() {
    let server = TestServer::start(|request| {
        assert_eq!(request.header("authorization"), Some("Bearer secret"));
        TestReply::json(&json!({"data": []}))
    })
    .await;
    let answers = Answers(Mutex::new(vec![format!("{}/v1/", server.url), " secret ".into()]), Mutex::default());
    let credential = login_llama(&answers, None, Some(client()), &CancellationToken::new()).await.unwrap();
    assert_eq!(credential.key.as_deref(), Some("secret"));
    assert_eq!(credential.env.as_ref().unwrap()["LLAMA_BASE_URL"], server.url);
    let prompts = answers.1.lock().unwrap().clone();
    assert_eq!(
        prompts[0],
        AuthPrompt::Text { message: "llama.cpp server URL".into(), placeholder: Some(DEFAULT_LLAMA_SERVER_URL.into()) }
    );
    let resolved = resolve_llama_auth(Some(&credential), &|_| None).unwrap().unwrap();
    assert_eq!(resolved.auth.api_key.as_deref(), Some("secret"));
    assert_eq!(resolved.auth.base_url, Some(format!("{}/v1", server.url)));
    assert_eq!(resolved.source.as_deref(), Some("stored credential"));
}
