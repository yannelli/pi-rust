//! The `/llama` view: screens, the Hugging Face search, progress, and the full flow through
//! [`LlamaView::bridge`] and [`LlamaManager`] against a local router.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::FutureExt;
use pi_tui::utils::visible_width;
use serde_json::json;

use crate::interactive_components::llama_view::{HuggingFaceSearchFn, LlamaView, LlamaViewUi};
use crate::interactive_components::test_support::{Calls, Harness, flush, keys, local};
use crate::llama::view::ProgressState;
use crate::llama::*;
use crate::platform::test_http::{TestReply, TestServer, client};

type View = Harness<LlamaView>;

fn view() -> View {
    Harness::new_shared(90, 30, |tui| Rc::new(RefCell::new(LlamaView::new(tui))))
}

fn bridge(harness: &View) -> (LlamaViewUi, Calls<(String, NotifyLevel)>) {
    let notes = Calls::new();
    let recorder = notes.clone();
    let ui = LlamaView::bridge(&harness.component, move |message, level| recorder.push((message.to_string(), level)));
    (ui, notes)
}

async fn wait_for(harness: &View, needle: &str) {
    for _ in 0..400 {
        flush(harness).await;
        if harness.screen().contains(needle) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {needle:?}:\n{}", harness.screen());
}

fn model(id: &str, downloads: u64) -> HuggingFaceModel {
    HuggingFaceModel { id: id.into(), downloads }
}

/// A search that records queries and answers from `answer`.
fn fake_search(
    queries: &Calls<String>,
    answer: fn(&str) -> Result<Vec<HuggingFaceModel>, String>,
) -> HuggingFaceSearchFn {
    let queries = queries.clone();
    Rc::new(move |query: String, _cancel| {
        queries.push(query.clone());
        async move { answer(&query) }.boxed_local()
    })
}

fn answers(query: &str) -> Result<Vec<HuggingFaceModel>, String> {
    match query {
        "qw" => Ok(vec![model("owner/qwen-a", 1_234), model("owner/qwen-b", 45_000_000)]),
        "many" => Ok((0..15).map(|index| model(&format!("owner/many-{index:02}"), index)).collect()),
        "zz" => Err("Hugging Face rate limit reached".into()),
        _ => Ok(Vec::new()),
    }
}

#[test]
fn starts_loading_and_lists_models_loaded_first() {
    let harness = view();
    assert!(harness.screen().contains("llama.cpp models") && harness.screen().contains("Loading…"));
    let models = [
        LlamaModelInfo::new("b-unloaded", "unloaded"),
        LlamaModelInfo::from_value(&json!({"id": "z-loaded", "status": {"value": "loaded"}, "meta": {"n_ctx": 65536}}))
            .unwrap(),
        LlamaModelInfo::new("a-loading", "loading"),
    ];
    let actions = Calls::new();
    harness.component.borrow_mut().show_models("http://router:8080", &models, actions.recorder());
    harness.settle();
    let screen = harness.screen();
    let rows: Vec<&str> = screen.lines().collect();
    assert!(rows[0].starts_with("────") && rows[1] == " llama.cpp models" && rows[2] == " http://router:8080");
    let order = ["→ z-loaded", "a-loading", "b-unloaded", "Download model…"].map(|id| screen.find(id).unwrap());
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{screen}");
    assert!(screen.contains("loaded · 66k context") && screen.contains("loading"));
    assert!(screen.contains("Hugging Face owner/repository[:quant]"));
    assert!(screen.contains("load/unload/download") && screen.contains("close"));
    harness.send_all(&[keys::ENTER, keys::ENTER]);
    let picked = actions.take();
    assert!(matches!(picked.as_slice(), [LlamaManagerAction::Model(model)] if model.id == "z-loaded"));

    harness.component.borrow_mut().show_models("http://router:8080", &models, actions.recorder());
    harness.send_all(&[keys::UP, keys::ENTER]);
    assert_eq!(actions.take(), [LlamaManagerAction::Download]);
    harness.component.borrow_mut().show_models("http://router:8080", &models, actions.recorder());
    harness.send(keys::ESCAPE);
    assert_eq!(actions.take(), [LlamaManagerAction::Close]);
}

#[test]
fn progress_shows_a_bar_and_stops_on_cancel() {
    let harness = view();
    let state = ProgressState {
        title: "Downloading model".into(),
        model: "owner/repo:Q4_K_M".into(),
        progress: LlamaProgress {
            message: "Downloading".into(),
            ratio: Some(0.5),
            detail: Some("1 GiB / 2 GiB".into()),
        },
    };
    let stops = Calls::new();
    harness.component.borrow_mut().progress(&state, stops.counter());
    harness.settle();
    let screen = harness.screen();
    assert!(screen.contains(" Downloading model") && screen.contains(" owner/repo:Q4_K_M"), "{screen}");
    assert!(screen.contains(&format!(" {}{} 50%", "█".repeat(20), "─".repeat(20))));
    assert!(screen.contains("1 GiB / 2 GiB") && screen.contains("stop"));
    let mut later = state.clone();
    later.progress.ratio = Some(0.75);
    later.progress.detail = None;
    harness.component.borrow_mut().update_progress(&later);
    harness.send(keys::DOWN);
    assert!(harness.screen().contains("75%") && !harness.screen().contains("1 GiB"));
    assert!(stops.is_empty() && harness.component.borrow().is_waiting_for_stop());
    harness.send_all(&[keys::ESCAPE, keys::ESCAPE]);
    assert_eq!(stops.len(), 1);
    assert!(!harness.component.borrow().is_waiting_for_stop());

    harness.component.borrow_mut().show_status("Loading model details", "owner/repo");
    harness.component.borrow_mut().update_progress(&state);
    harness.settle();
    assert!(harness.screen().contains("Loading model details") && !harness.screen().contains("50%"));
}

#[test]
fn lines_never_exceed_the_width() {
    let harness = view();
    let long = "x".repeat(200);
    harness.component.borrow_mut().select(&long, std::slice::from_ref(&long), |_| {});
    for width in [10, 24, 60] {
        assert!(harness.render(width).iter().all(|line| visible_width(line) <= width));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn search_debounces_caches_filters_and_selects() {
    local(async {
        let harness = view();
        let (queries, choice) = (Calls::new(), Calls::new());
        harness.component.borrow_mut().search_models(fake_search(&queries, answers), choice.recorder());
        harness.settle();
        let screen = harness.screen();
        assert!(screen.contains(" Download model") && screen.contains("Model name or owner/repository[:quant]"));
        assert!(screen.contains("  Type at least 2 characters") && screen.contains("back"), "{screen}");
        harness.type_text("q");
        assert!(harness.screen().contains("Type at least 2 characters"));
        harness.type_text("w");
        assert!(harness.screen().contains("  Searching Hugging Face…") && queries.is_empty());
        harness.run_for(Duration::from_millis(550));
        flush(&harness).await;
        assert_eq!(queries.get(), ["qw"]);
        let screen = harness.screen();
        assert!(screen.contains("→ owner/qwen-a  1.2k downloads"), "{screen}");
        assert!(screen.contains("  owner/qwen-b  45M downloads") && !screen.contains("Searching"));

        harness.type_text("b");
        let screen = harness.screen();
        assert!(screen.contains("→ owner/qwen-b") && !screen.contains("qwen-a"), "{screen}");
        assert!(screen.contains("Searching Hugging Face…"));
        harness.send(keys::BACKSPACE);
        harness.run_for(Duration::from_millis(550));
        flush(&harness).await;
        assert_eq!(queries.get(), ["qw"], "cached query and cleared debounce");
        harness.send_all(&[keys::DOWN, keys::DOWN, keys::UP]);
        assert!(harness.screen().contains("→ owner/qwen-b"));
        harness.send(keys::ENTER);
        assert_eq!(choice.get(), [Some("owner/qwen-b".to_string())]);
        harness.send(keys::ESCAPE);
        assert_eq!(choice.len(), 1);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn search_shows_errors_empty_results_scrolling_and_exact_names() {
    local(async {
        let harness = view();
        let (queries, choice) = (Calls::new(), Calls::new());
        let search = fake_search(&queries, answers);
        harness.component.borrow_mut().search_models(search.clone(), choice.recorder());
        for (query, expected) in [("zz", "  Hugging Face rate limit reached"), ("none", "  No GGUF models found")] {
            harness.send_all(&[keys::BACKSPACE; 4]);
            harness.type_text(query);
            harness.run_for(Duration::from_millis(550));
            flush(&harness).await;
            assert!(harness.screen().contains(expected), "{}", harness.screen());
        }
        harness.send(keys::ENTER);
        assert!(choice.is_empty(), "nothing to select");
        harness.send_all(&[keys::BACKSPACE; 4]);
        harness.type_text("many");
        harness.run_for(Duration::from_millis(550));
        flush(&harness).await;
        let screen = harness.screen();
        assert!(screen.contains("owner/many-09") && !screen.contains("owner/many-10"), "{screen}");
        assert!(screen.contains("  (1/15)"));
        harness.send(keys::UP);
        let screen = harness.screen();
        assert!(screen.contains("→ owner/many-14") && screen.contains("(15/15)") && !screen.contains("many-04"));
        harness.send(keys::ESCAPE);
        assert_eq!(choice.take(), [None]);

        harness.component.borrow_mut().search_models(search, choice.recorder());
        harness.type_text("owner/repo:Q8_0");
        harness.send(keys::ENTER);
        assert_eq!(choice.take(), [Some("owner/repo:Q8_0".to_string())]);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn bridge_serves_selects_confirms_status_and_notices() {
    local(async {
        let harness = view();
        let (ui, notes) = bridge(&harness);
        let answer: Rc<RefCell<Vec<String>>> = Rc::default();
        let (task_ui, slot) = (ui.clone(), answer.clone());
        tokio::task::spawn_local(async move {
            let confirmed = task_ui.confirm("Unload model?", "qwen").await;
            slot.borrow_mut().push(confirmed.to_string());
            let choice = task_ui.connection_error("http://router", "Could not connect to the server.").await;
            slot.borrow_mut().push(format!("{choice:?}"));
            let choice = task_ui.connection_error("http://router", "again").await;
            slot.borrow_mut().push(format!("{choice:?}"));
        });
        wait_for(&harness, "Unload model?").await;
        let screen = harness.screen();
        assert!(screen.contains(" qwen") && screen.contains("→ Yes") && screen.contains("  No"), "{screen}");
        harness.send(keys::ENTER);
        wait_for(&harness, "llama.cpp unavailable").await;
        let rows = harness.viewport();
        let title = rows.iter().position(|row| row == " llama.cpp unavailable").unwrap();
        assert_eq!(rows[title + 1..title + 4], [" http://router", "", " Could not connect to the server."]);
        harness.send(keys::ENTER);
        wait_for(&harness, "again").await;
        harness.send(keys::ESCAPE);
        flush(&harness).await;
        assert_eq!(*answer.borrow(), ["true", "Retry", "Close"]);

        ui.show_status("Loading model details", "owner/repo");
        ui.notify("Loaded qwen", NotifyLevel::Info);
        wait_for(&harness, "Loading model details").await;
        assert_eq!(notes.get(), [("Loaded qwen".to_string(), NotifyLevel::Info)]);
        let detached = Rc::new(RefCell::new(LlamaView::new(&harness.tui)));
        let orphan = LlamaView::bridge(&detached, |_, _| {});
        drop(detached);
        assert_eq!(orphan.show_models("http://router", &[]).await, LlamaManagerAction::Close, "view gone");
    })
    .await;
}

struct NoSync;

#[async_trait]
impl LlamaCatalogSync for NoSync {
    async fn refresh_llama_models(&self) -> Result<(), String> {
        Ok(())
    }
}

/// A router whose loads, unloads, and downloads take effect at once, except `slow` loads.
async fn router(models: &[(&str, &str)]) -> TestServer {
    let state = Arc::new(Mutex::new(models.iter().map(|(id, status)| (id.to_string(), status.to_string())).collect()));
    TestServer::start(move |request| {
        let mut models: std::sync::MutexGuard<Vec<(String, String)>> = state.lock().unwrap();
        let target = request.json()["model"].as_str().unwrap_or_default().to_string();
        let set = |models: &mut Vec<(String, String)>, status: &str| {
            models.iter_mut().filter(|model| model.0 == target).for_each(|model| model.1 = status.into());
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
    .await
}

/// Runs the `/llama` manager against `server` on a local task; the flag turns on when it ends.
fn run_manager(server: &TestServer, ui: LlamaViewUi) -> Rc<Cell<bool>> {
    let done = Rc::new(Cell::new(false));
    let (url, flag) = (server.url.clone(), done.clone());
    tokio::task::spawn_local(async move {
        let provider = LlamaProvider::new();
        let manager = LlamaManager {
            client: LlamaClient::new(&url, None).unwrap().with_http(client()),
            provider: &provider,
            ui: &ui,
            sync: &NoSync,
            hugging_face: HuggingFaceClient::new(None, Some(&url)).with_http(client()),
        };
        manager.run().await;
        flag.set(true);
    });
    done
}

#[tokio::test(flavor = "current_thread")]
async fn downloads_through_search_gated_warning_and_quantization() {
    local(async {
        let server = router(&[]).await;
        let harness = view();
        let (ui, notes) = bridge(&harness);
        let done = run_manager(&server, ui);
        wait_for(&harness, "Download model…").await;
        harness.send(keys::ENTER);
        wait_for(&harness, "Type at least 2 characters").await;
        harness.type_text("repo");
        harness.run_for(Duration::from_millis(550));
        wait_for(&harness, "→ owner/repo  10 downloads").await;
        harness.send(keys::ENTER);
        wait_for(&harness, "Hugging Face access required").await;
        let screen = harness.screen();
        assert!(screen.contains("Accept the access terms at:") && screen.contains("https://huggingface.co/owner/repo"));
        assert!(screen.contains("The llama.cpp server needs HF_TOKEN with access.") && screen.contains("→ Continue"));
        harness.send(keys::ENTER);
        wait_for(&harness, "Select quantization").await;
        assert!(harness.screen().contains("→ Q4_K_M · 4.00 KiB · recommended"), "{}", harness.screen());
        harness.send(keys::ENTER);
        wait_for(&harness, "load/unload/download").await;
        assert!(harness.screen().contains("owner/repo:Q4_K_M"));
        assert!(notes.get().contains(&("Downloaded owner/repo:Q4_K_M".to_string(), NotifyLevel::Info)));
        harness.send(keys::ESCAPE);
        flush(&harness).await;
        assert!(done.get());
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn stopping_a_load_asks_first_and_unloads() {
    local(async {
        let server = router(&[("a", "loaded"), ("slow", "unloaded")]).await;
        let harness = view();
        let (ui, _notes) = bridge(&harness);
        let done = run_manager(&server, ui);
        wait_for(&harness, "→ a").await;
        harness.send_all(&[keys::DOWN, keys::ENTER]);
        wait_for(&harness, "1 model is loaded").await;
        harness.send_all(&[keys::DOWN, keys::ENTER]);
        wait_for(&harness, "Loading model").await;
        assert!(harness.screen().contains(" slow") && harness.screen().contains("stop"));
        harness.send(keys::ESCAPE);
        wait_for(&harness, "Stop loading?").await;
        harness.send_all(&[keys::DOWN, keys::ENTER]);
        wait_for(&harness, "Loading model").await;
        harness.send(keys::ESCAPE);
        wait_for(&harness, "Stop loading?").await;
        harness.send(keys::ENTER);
        wait_for(&harness, "load/unload/download").await;
        let unloads = server.requests();
        assert!(unloads.iter().any(|request| request.path() == "/models/unload" && request.json()["model"] == "slow"));
        harness.send(keys::ESCAPE);
        flush(&harness).await;
        assert!(done.get());
    })
    .await;
}
