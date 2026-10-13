//! Virtual models in sessions (upstream `test/suite/virtual-models.test.ts` and the
//! `createAgentSession` part of `test/virtual-models.test.ts`). `local/tiny` reasons and plays
//! the large model; `local/other` does not and plays the small one.

use std::sync::{Arc, Mutex};

use pi_ai::providers::faux::FauxResponse;
use pi_ai::{Message, StopReason, ThinkingLevel};
use pi_coding_agent::session::SessionManager;
use pi_coding_agent::virtual_models::{
    ModelLookup, ModelRoute, ModelRouteReason, ModelRouteRequest, ModelRouter, VIRTUAL_MODEL_STATE_ENTRY,
    VirtualModelDefinition,
};
use serde_json::{Value, json};

use crate::harness::{Harness, Options, harness, open_session};

type Route = Arc<dyn Fn(&ModelRouteRequest, &dyn ModelLookup) -> anyhow::Result<ModelRoute> + Send + Sync>;

/// New user turns pick by thinking level; everything else stays on the model that answered.
fn default_route(request: &ModelRouteRequest, models: &dyn ModelLookup) -> anyhow::Result<ModelRoute> {
    let find = |id: &str| models.find("local", id).unwrap();
    let route = |id: &str, thinking_level| ModelRoute { model: find(id), thinking_level, state: None };
    if request.reason == ModelRouteReason::Direct {
        return Ok(route("tiny", ThinkingLevel::Low));
    }
    let sticky = request
        .failed
        .as_ref()
        .map(|failed| (failed.model.clone(), failed.thinking_level))
        .or_else(|| request.previous.as_ref().map(|previous| (previous.model.clone(), previous.thinking_level)));
    if request.reason != ModelRouteReason::User
        && let Some((model, level)) = sticky
    {
        return Ok(ModelRoute { model, thinking_level: level.unwrap_or(ThinkingLevel::High), state: None });
    }
    Ok(match request.thinking_level {
        ThinkingLevel::High => route("tiny", ThinkingLevel::High),
        _ => route("other", ThinkingLevel::Off),
    })
}

struct Recorder {
    route: Route,
    requests: Mutex<Vec<ModelRouteRequest>>,
}

#[async_trait::async_trait]
impl ModelRouter for Recorder {
    async fn route(&self, request: ModelRouteRequest, models: &dyn ModelLookup) -> anyhow::Result<ModelRoute> {
        self.requests.lock().unwrap().push(request.clone());
        (self.route)(&request, models)
    }
}

struct Routed {
    h: Harness,
    recorder: Arc<Recorder>,
}

impl Routed {
    fn reasons(&self) -> Vec<&'static str> {
        self.recorder.requests.lock().unwrap().iter().map(|request| request.reason.as_str()).collect()
    }

    fn requests(&self) -> Vec<ModelRouteRequest> {
        self.recorder.requests.lock().unwrap().clone()
    }

    /// Physical model and thinking level recorded on each response.
    fn dispatched(&self) -> Vec<String> {
        let messages = self.h.session.messages();
        messages
            .iter()
            .filter_map(|message| match message {
                Message::Assistant(assistant) => Some(format!(
                    "{}/{}:{}",
                    assistant.provider,
                    assistant.model,
                    assistant.thinking_level.map_or("-", |level| level.as_str())
                )),
                _ => None,
            })
            .collect()
    }

    fn stored_states(&self) -> Vec<Value> {
        let manager = self.h.session.session_manager();
        let branch = manager.get_branch(None);
        branch
            .iter()
            .filter_map(|entry| {
                let raw = entry.raw();
                (raw["type"] == "custom" && raw["customType"] == VIRTUAL_MODEL_STATE_ENTRY).then(|| raw["data"].clone())
            })
            .collect()
    }
}

fn definition(router: Arc<dyn ModelRouter>) -> VirtualModelDefinition {
    let mut definition = VirtualModelDefinition::new("router", "auto", "Auto", router);
    definition.thinking_levels = Some(vec![ThinkingLevel::Low, ThinkingLevel::High]);
    definition.context_window = Some(1000);
    definition
}

async fn routed(route: Route, options: Options) -> Routed {
    let h = harness(Options { files: vec![("notes.txt", "meeting on Friday")], ..options }).await;
    let recorder = Arc::new(Recorder { route, requests: Mutex::new(Vec::new()) });
    h.session.register_virtual_model(definition(recorder.clone())).unwrap();
    let model = h.session.model_runtime().get_model("router", "auto").unwrap();
    h.session.set_model(model, false).await.unwrap();
    h.session.set_thinking_level(ThinkingLevel::High, false);
    Routed { h, recorder }
}

fn read_call() -> FauxResponse {
    FauxResponse::tool_call("r1", "read", json!({"path": "notes.txt"}))
}

fn retry_settings() -> Value {
    json!({"retry": {"enabled": true, "maxRetries": 3, "baseDelayMs": 1}})
}

#[tokio::test]
async fn routes_each_request_including_retries_while_the_selection_stays_virtual() {
    let r = routed(
        Arc::new(default_route),
        Options {
            responses: vec![FauxResponse::Error("overloaded_error".into()), read_call(), FauxResponse::text("done")],
            settings: retry_settings(),
            context_window: 50_000,
            ..Options::default()
        },
    )
    .await;
    r.h.session.prompt("hello", Vec::new()).await.unwrap();

    assert_eq!(r.reasons(), ["user", "retry", "continuation"]);
    let requests = r.requests();
    let failed = requests[1].failed.as_ref().unwrap();
    assert_eq!(failed.message.error_message.as_deref(), Some("overloaded_error"));
    assert_eq!(failed.model.id, "tiny");
    assert_eq!(requests[2].previous.as_ref().unwrap().model.id, "tiny");
    assert_eq!(r.dispatched(), ["local/tiny:high", "local/tiny:high"]);
    let model = r.h.session.model().unwrap();
    assert_eq!((model.provider.as_str(), model.id.as_str()), ("router", "auto"));
    assert_eq!(r.h.session.thinking_level(), ThinkingLevel::High);
    // Limits come from the physical model of the latest response.
    assert_eq!(r.h.session.context_usage().unwrap().context_window, 50_000);
    assert_eq!(r.h.session.routed_model().unwrap().model.id, "tiny");
    let routed_events = r.h.events_of("model_routed");
    let reasons: Vec<&str> = routed_events.iter().map(|event| event["reason"].as_str().unwrap()).collect();
    assert_eq!(reasons, ["user", "retry", "continuation"]);
    let first = (routed_events[0]["model"]["id"].as_str(), routed_events[0]["thinkingLevel"].as_str());
    assert_eq!(first, (Some("tiny"), Some("high")));
}

#[tokio::test]
async fn retries_the_first_request_of_a_turn_on_the_model_routed_for_it() {
    let r = routed(
        Arc::new(default_route),
        Options {
            responses: vec![
                FauxResponse::text("easy answer"),
                FauxResponse::Error("overloaded_error".into()),
                FauxResponse::text("hard answer"),
            ],
            settings: retry_settings(),
            ..Options::default()
        },
    )
    .await;
    r.h.session.set_thinking_level(ThinkingLevel::Low, false);
    r.h.session.prompt("easy", Vec::new()).await.unwrap();
    r.h.session.set_thinking_level(ThinkingLevel::High, false);
    r.h.session.prompt("hard", Vec::new()).await.unwrap();

    assert_eq!(r.reasons(), ["user", "user", "retry"]);
    let requests = r.requests();
    assert_eq!(requests[2].failed.as_ref().unwrap().model.id, "tiny");
    assert_eq!(requests[2].previous.as_ref().unwrap().model.id, "other");
    assert_eq!(r.dispatched(), ["local/other:off", "local/tiny:high"]);
}

#[tokio::test]
async fn ends_the_run_with_an_error_response_when_routing_fails() {
    let fail = Arc::new(Mutex::new(false));
    let flag = fail.clone();
    let route: Route = Arc::new(move |request, models| {
        anyhow::ensure!(!*flag.lock().unwrap(), "router unavailable");
        default_route(request, models)
    });
    let r = routed(
        route,
        Options { responses: vec![FauxResponse::text("answer")], context_window: 50_000, ..Options::default() },
    )
    .await;
    r.h.session.prompt("hello", Vec::new()).await.unwrap();
    *fail.lock().unwrap() = true;
    r.h.session.prompt("again", Vec::new()).await.unwrap();

    let messages = r.h.session.messages();
    let Some(Message::Assistant(last)) = messages.last() else { panic!("assistant response") };
    assert_eq!((last.provider.as_str(), last.model.as_str()), ("router", "auto"));
    assert_eq!(last.stop_reason, StopReason::Error);
    assert!(last.error_message.as_deref().unwrap().contains("router unavailable"));
    assert_eq!(r.h.requests().len(), 1);
    // The failed request names the virtual model (1k window); the 50k of the model that answered applies.
    assert_eq!(r.h.session.context_usage().unwrap().context_window, 50_000);
}

#[tokio::test]
async fn rejects_routes_to_virtual_models() {
    let route: Route = Arc::new(|request, _| {
        Ok(ModelRoute { model: request.model.clone(), thinking_level: ThinkingLevel::Off, state: None })
    });
    let r = routed(route, Options::default()).await;
    r.h.session.prompt("hello", Vec::new()).await.unwrap();
    let messages = r.h.session.messages();
    let Some(Message::Assistant(last)) = messages.last() else { panic!("assistant response") };
    assert_eq!(
        last.error_message.as_deref(),
        Some("Virtual model router/auto routed to router/auto, which is not a physical model.")
    );
    assert!(r.h.requests().is_empty());
}

#[tokio::test]
async fn stores_router_state_on_the_branch_and_passes_it_back() {
    let states: Arc<Mutex<Vec<Option<Value>>>> = Arc::default();
    let seen = states.clone();
    let route: Route = Arc::new(move |request, models| {
        seen.lock().unwrap().push(request.state.clone());
        let turns = request.state.as_ref().and_then(|state| state["turns"].as_u64()).unwrap_or(0);
        let mut route = default_route(request, models)?;
        // Continuations keep the state; direct requests neither get nor store it.
        if request.reason != ModelRouteReason::Continuation {
            route.state = Some(json!({"turns": turns + 1}));
        }
        Ok(route)
    });
    let summary = "## Goal\nSummary";
    let r = routed(
        route,
        Options {
            responses: vec![
                read_call(),
                FauxResponse::text("first"),
                FauxResponse::text("second"),
                FauxResponse::text(summary),
                FauxResponse::text(summary),
                FauxResponse::text("third"),
            ],
            settings: json!({"compaction": {"keepRecentTokens": 1}}),
            ..Options::default()
        },
    )
    .await;
    r.h.session.prompt("one", Vec::new()).await.unwrap();
    r.h.session.prompt("two", Vec::new()).await.unwrap();
    let state = |turns: u64| json!({"provider": "router", "modelId": "auto", "state": {"turns": turns}});
    assert_eq!(r.stored_states(), [state(1), state(2)]);

    r.h.session.compact(None).await.unwrap();

    assert_eq!(r.reasons(), ["user", "continuation", "user", "direct"]);
    let turns = |value: u64| Some(json!({"turns": value}));
    assert_eq!(*states.lock().unwrap(), [None, turns(1), turns(1), None]);
    assert_eq!(r.stored_states(), [state(1), state(2)], "direct requests store no state");

    // The state follows the branch on resume.
    let file = r.h.session.session_file().unwrap();
    let resumed = open_session(&r.h.services, SessionManager::open(&file).unwrap()).await;
    assert_eq!(resumed.model().unwrap().id, "auto", "the virtual selection is restored");
    resumed.prompt("three", Vec::new()).await.unwrap();
    assert_eq!(states.lock().unwrap().last().unwrap(), &turns(2));
}

#[tokio::test]
async fn restores_the_virtual_selection_or_falls_back_to_the_physical_model() {
    let options = Options { responses: vec![FauxResponse::text("answer")], ..Options::default() };
    let r = routed(Arc::new(default_route), options).await;
    r.h.session.prompt("hello", Vec::new()).await.unwrap();
    let file = r.h.session.session_file().unwrap();
    let resumed = open_session(&r.h.services, SessionManager::open(&file).unwrap()).await;
    assert_eq!(resumed.model().unwrap().id, "auto");
    assert_eq!(resumed.routed_model().unwrap().model.id, "tiny");

    assert!(r.h.session.unregister_virtual_model("router", "auto"));
    assert!(r.h.session.model_runtime().get_model("router", "auto").is_none());
    let resumed = open_session(&r.h.services, SessionManager::open(&file).unwrap()).await;
    assert_eq!(resumed.model().unwrap().id, "tiny", "an unregistered virtual model falls back to the answer");
    assert!(resumed.routed_model().is_none());
}

#[tokio::test]
async fn lists_virtual_models_and_rejects_conflicts() {
    let r = routed(Arc::new(default_route), Options::default()).await;
    let runtime = r.h.session.model_runtime();
    assert!(runtime.available_models().iter().any(|model| model.provider == "router" && model.id == "auto"));
    let conflict = VirtualModelDefinition::new("local", "tiny", "Tiny", r.recorder.clone());
    let error = r.h.session.register_virtual_model(conflict).unwrap_err().to_string();
    assert_eq!(error, "Virtual model local/tiny conflicts with a physical model.");
    let empty = VirtualModelDefinition::new(" ", "x", "X", r.recorder.clone());
    assert!(r.h.session.register_virtual_model(empty).is_err());
    // On a provider with physical models, the virtual model needs that provider's credentials.
    let listed = VirtualModelDefinition::new("local", "auto", "Auto", r.recorder.clone());
    r.h.session.register_virtual_model(listed).unwrap();
    assert!(runtime.available_models().iter().any(|model| model.provider == "local" && model.id == "auto"));
}
