//! Port of the `getBranchSelection` and catalog parts of upstream `test/virtual-models.test.ts`.

use std::cell::RefCell;
use std::path::Path;

use pi_ai::models::supported_thinking_levels;

use super::*;
use crate::session::SessionManager;

fn physical(id: &str) -> Model {
    Model { id: id.into(), name: id.into(), api: "faux".into(), provider: "faux".into(), ..Model::default() }
}

fn router(_: &ModelRouteRequest, models: &dyn ModelLookup) -> anyhow::Result<ModelRoute> {
    let model = models.find("faux", "small").ok_or_else(|| anyhow::anyhow!("missing"))?;
    Ok(ModelRoute { model, thinking_level: ThinkingLevel::Off, state: None })
}

fn virtual_model() -> Model {
    let mut definition = VirtualModelDefinition::new("router", "auto", "Auto", Arc::new(router));
    definition.thinking_levels = Some(vec![ThinkingLevel::Low, ThinkingLevel::High]);
    create_virtual_model(&definition)
}

fn answer(model: &Model, stop_reason: StopReason) -> Message {
    Message::Assistant(AssistantMessage {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        stop_reason,
        ..AssistantMessage::default()
    })
}

/// The selection of a branch built by `build`, and the catalog lookups it made.
fn select(build: impl FnOnce(&mut SessionManager)) -> (Option<(String, String)>, Vec<String>) {
    let mut session = SessionManager::in_memory(Path::new("/tmp"));
    build(&mut session);
    let lookups = RefCell::new(Vec::new());
    let get_model = |provider: &str, id: &str| {
        lookups.borrow_mut().push(format!("{provider}/{id}"));
        match (provider, id) {
            ("router", "auto") => Some(virtual_model()),
            ("faux", id) => Some(physical(id)),
            _ => None,
        }
    };
    let selection = get_branch_selection(&session.get_branch(None), &get_model);
    (selection, lookups.into_inner())
}

fn pair(provider: &str, id: &str) -> Option<(String, String)> {
    Some((provider.into(), id.into()))
}

#[test]
fn branch_selection_looks_up_only_the_last_model_change() {
    let (selection, lookups) = select(|session| {
        session.append_model_change("faux", "small").unwrap();
        for _ in 0..100 {
            session.append_message(&answer(&physical("large"), StopReason::Stop)).unwrap();
        }
    });
    assert_eq!((selection, lookups), (pair("faux", "large"), vec!["faux/small".to_string()]));

    let (selection, lookups) = select(|session| {
        session.append_model_change("faux", "small").unwrap();
        session.append_message(&answer(&physical("small"), StopReason::Stop)).unwrap();
        session.append_model_change("router", "auto").unwrap();
        for _ in 0..100 {
            session.append_message(&answer(&physical("large"), StopReason::Stop)).unwrap();
        }
    });
    assert_eq!((selection, lookups), (pair("router", "auto"), vec!["router/auto".to_string()]));
}

#[test]
fn branch_selection_uses_the_last_change_or_the_latest_physical_response() {
    let (selection, lookups) = select(|session| {
        session.append_model_change("router", "auto").unwrap();
        session.append_message(&answer(&physical("small"), StopReason::Stop)).unwrap();
        session.append_model_change("faux", "small").unwrap();
    });
    assert_eq!((selection, lookups), (pair("faux", "small"), Vec::new()));

    let (selection, lookups) = select(|session| {
        session.append_message(&answer(&physical("small"), StopReason::Stop)).unwrap();
        session.append_message(&answer(&physical("large"), StopReason::Stop)).unwrap();
        session.append_message(&answer(&virtual_model(), StopReason::Error)).unwrap();
    });
    assert_eq!((selection, lookups), (pair("faux", "large"), Vec::new()));

    let (selection, _) = select(|session| {
        session.append_model_change("router", "gone").unwrap();
        session.append_message(&answer(&physical("large"), StopReason::Stop)).unwrap();
    });
    assert_eq!(selection, pair("faux", "large"), "an unregistered virtual model does not hold");
}

#[test]
fn router_state_follows_the_branch() {
    let mut session = SessionManager::in_memory(Path::new("/tmp"));
    assert_eq!(get_virtual_model_state(&session.get_branch(None), "router", "auto"), None);
    let data = virtual_model_state_data("router", "auto", &json!({"phase": "plan"}));
    session.append_custom_entry(VIRTUAL_MODEL_STATE_ENTRY, Some(data)).unwrap();
    let other = virtual_model_state_data("router", "other", &json!({"phase": "x"}));
    session.append_custom_entry(VIRTUAL_MODEL_STATE_ENTRY, Some(other)).unwrap();
    let plan = session.leaf_id().map(String::from);
    let data = virtual_model_state_data("router", "auto", &json!({"phase": "build"}));
    session.append_custom_entry(VIRTUAL_MODEL_STATE_ENTRY, Some(data)).unwrap();
    assert_eq!(get_virtual_model_state(&session.get_branch(None), "router", "auto"), Some(json!({"phase": "build"})));
    assert_eq!(
        get_virtual_model_state(&session.get_branch(plan.as_deref()), "router", "auto"),
        Some(json!({"phase": "plan"}))
    );
}

#[test]
fn creates_catalog_entries_and_finds_the_latest_response() {
    let model = virtual_model();
    assert_eq!((model.api.as_str(), model.context_window, model.max_tokens), (VIRTUAL_MODEL_API, 0, 0));
    assert_eq!(model.input, ["text", "image"]);
    assert!(is_virtual_model(&model));
    assert_eq!(supported_thinking_levels(&model), [ThinkingLevel::Low, ThinkingLevel::High]);
    let plain = create_virtual_model(&VirtualModelDefinition::new("p", "id", "Name", Arc::new(router)));
    assert_eq!(supported_thinking_levels(&plain), [ThinkingLevel::Off]);

    let messages = [
        answer(&physical("small"), StopReason::Stop),
        answer(&physical("large"), StopReason::ToolUse),
        answer(&model, StopReason::Error),
        answer(&physical("x"), StopReason::Aborted),
    ];
    assert_eq!(find_latest_response(&messages).unwrap().model, "large");
}

#[test]
fn requests_serialize_in_the_upstream_shape() {
    let failed = AssistantMessage { error_message: Some("overloaded".into()), ..AssistantMessage::default() };
    let request = ModelRouteRequest {
        model: virtual_model(),
        thinking_level: ThinkingLevel::High,
        reason: ModelRouteReason::Retry,
        previous: Some(RoutedModel { model: physical("small"), thinking_level: None }),
        failed: Some(FailedRequest {
            model: physical("large"),
            thinking_level: Some(ThinkingLevel::High),
            message: failed,
        }),
        state: Some(json!({"phase": "plan"})),
        messages: Vec::new(),
        cancel: CancellationToken::new(),
    };
    let value = request.to_json();
    assert_eq!((value["reason"].as_str(), value["thinkingLevel"].as_str()), (Some("retry"), Some("high")));
    assert_eq!(value["previous"]["model"]["id"], "small");
    assert!(value["previous"].get("thinkingLevel").is_none());
    assert_eq!(value["failed"]["thinkingLevel"], "high");
    assert_eq!(value["failed"]["message"]["errorMessage"], "overloaded");
    assert_eq!(value["state"], json!({"phase": "plan"}));
}
