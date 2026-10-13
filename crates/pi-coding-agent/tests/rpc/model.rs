//! Model, thinking level, queue mode, compaction, and retry settings (upstream `rpc.test.ts`
//! model and thinking cases).

use serde_json::{Value, json};

use crate::harness::Options;
use crate::support::start;

/// `provider/id` of the `local` models; credentials in the environment can add others.
fn local_ids(models: &Value) -> Vec<String> {
    let local = models.as_array().unwrap().iter().filter(|model| model["provider"] == "local");
    local.map(|model| format!("local/{}", model["id"].as_str().unwrap())).collect()
}

#[tokio::test]
async fn lists_sets_and_cycles_models_without_saving_them() {
    let mut rpc = start(Options::default()).await;
    let models = rpc.data(json!({"id": "l", "type": "get_available_models"})).await["models"].clone();
    assert_eq!(local_ids(&models), ["local/tiny", "local/other"]);
    for model in models.as_array().unwrap() {
        assert!(model["contextWindow"].as_u64().unwrap() > 0 && model["reasoning"].is_boolean(), "{model}");
    }

    let set = rpc.data(json!({"id": "s", "type": "set_model", "provider": "local", "modelId": "other"})).await;
    assert_eq!(
        (set["id"].clone(), set["provider"].clone(), set["maxTokens"].clone()),
        (json!("other"), json!("local"), json!(4000))
    );
    assert!(set["api"].as_str().unwrap().starts_with("faux-session-") && set.get("baseUrl").is_some());
    assert_eq!(rpc.data(json!({"id": "st", "type": "get_state"})).await["model"]["id"], "other");
    assert_eq!(rpc.h.settings.default_model().as_deref(), Some("tiny"), "RPC model changes are not saved");

    let missing = rpc.request(json!({"id": "x", "type": "set_model", "provider": "local", "modelId": "nope"})).await;
    assert_eq!(
        missing,
        json!({"id": "x", "type": "response", "command": "set_model", "success": false, "error": "Model not found: local/nope"})
    );

    rpc.data(json!({"id": "s2", "type": "set_model", "provider": "local", "modelId": "tiny"})).await;
    let cycled = rpc.data(json!({"id": "c", "type": "cycle_model"})).await;
    assert_eq!((cycled["model"]["id"].clone(), cycled["isScoped"].clone()), (json!("other"), json!(false)));
    assert!(cycled["thinkingLevel"].is_string());
    rpc.finish().await;
}

#[tokio::test]
async fn sets_cycles_and_lists_thinking_levels() {
    let mut rpc = start(Options::default()).await;
    let levels = rpc.data(json!({"id": "l", "type": "get_available_thinking_levels"})).await["levels"].clone();
    let levels: Vec<String> = serde_json::from_value(levels).unwrap();
    assert_eq!(levels[..5], ["off", "minimal", "low", "medium", "high"]);
    let state = rpc.data(json!({"id": "s0", "type": "get_state"})).await;
    assert!(levels.contains(&state["thinkingLevel"].as_str().unwrap().to_string()));

    let set = rpc.request(json!({"id": "h", "type": "set_thinking_level", "level": "high"})).await;
    assert_eq!(set, json!({"id": "h", "type": "response", "command": "set_thinking_level", "success": true}));
    assert_eq!(rpc.data(json!({"id": "s1", "type": "get_state"})).await["thinkingLevel"], "high");
    let changed = rpc.wait_for(|record| record["type"] == "thinking_level_changed").await;
    assert_eq!(changed, json!({"type": "thinking_level_changed", "level": "high"}));

    let cycled = rpc.data(json!({"id": "c", "type": "cycle_thinking_level"})).await;
    let level = cycled["level"].as_str().unwrap().to_string();
    assert!(levels.contains(&level) && level != "high", "{cycled}");
    assert_eq!(rpc.data(json!({"id": "s2", "type": "get_state"})).await["thinkingLevel"], level);

    rpc.data(json!({"id": "m", "type": "set_model", "provider": "local", "modelId": "other"})).await;
    assert_eq!(rpc.data(json!({"id": "c2", "type": "cycle_thinking_level"})).await, Value::Null);
    assert_eq!(
        rpc.data(json!({"id": "l2", "type": "get_available_thinking_levels"})).await,
        json!({"levels": ["off"]})
    );

    let invalid = rpc.request(json!({"id": "bad", "type": "set_thinking_level", "level": "extreme"})).await;
    assert_eq!((invalid["success"].clone(), invalid["command"].clone()), (json!(false), json!("set_thinking_level")));
    rpc.finish().await;
}

#[tokio::test]
async fn sets_queue_modes_and_automatic_behavior() {
    let mut rpc = start(Options::default()).await;
    let state = rpc.data(json!({"id": "s0", "type": "get_state"})).await;
    assert_eq!(
        (state["steeringMode"].clone(), state["followUpMode"].clone()),
        (json!("one-at-a-time"), json!("one-at-a-time"))
    );
    assert_eq!(state["autoCompactionEnabled"], true);

    for (id, kind) in [("a", "set_steering_mode"), ("b", "set_follow_up_mode")] {
        let response = rpc.request(json!({"id": id, "type": kind, "mode": "all"})).await;
        assert_eq!(response, json!({"id": id, "type": "response", "command": kind, "success": true}));
    }
    let off = rpc.request(json!({"id": "c", "type": "set_auto_compaction", "enabled": false})).await;
    assert_eq!(off["success"], true);
    let state = rpc.data(json!({"id": "s1", "type": "get_state"})).await;
    assert_eq!((state["steeringMode"].clone(), state["followUpMode"].clone()), (json!("all"), json!("all")));
    assert_eq!(state["autoCompactionEnabled"], false);
    assert_eq!(rpc.data(json!({"id": "r", "type": "set_auto_retry", "enabled": false})).await, Value::Null);
    assert!(!rpc.h.settings.retry_enabled());
    assert_eq!(rpc.data(json!({"id": "ar", "type": "abort_retry"})).await, Value::Null);

    let bad = rpc.request(json!({"id": "m", "type": "set_steering_mode", "mode": "sometimes"})).await;
    assert_eq!(bad["success"], false);
    rpc.finish().await;
}
