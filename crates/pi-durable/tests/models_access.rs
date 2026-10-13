//! Model access that has no upstream test file of its own: faux deferred responses, `RegistryModels` routing and
//! setup errors, `uuidv7`, and tool argument validation.

use pi_ai::{Context, Message, Model, StopReason, Tool, ToolCall};
use pi_durable::models::{
    DeferredFetchOptions, DeferredOption, Models, RegistryModels, SimpleStreamOptions, create_models,
};
use pi_durable::testing::faux::{
    FauxDeferredOptions, FauxMessageOptions, RegisterFauxProviderOptions, faux_assistant_message, faux_provider,
};
use pi_durable::util::{uuidv7, uuidv7_at, validate_tool_arguments};
use serde_json::json;

fn hi() -> Context {
    Context { messages: vec![Message::user("hi")], ..Context::default() }
}

#[tokio::test]
async fn faux_deferred_responses_poll_then_resolve_and_cancel() {
    let faux = faux_provider(RegisterFauxProviderOptions {
        deferred: Some(FauxDeferredOptions { pending_fetches: 1, poll_after_ms: Some(250) }),
        ..RegisterFauxProviderOptions::default()
    });
    faux.set_responses([
        faux_assistant_message("later", FauxMessageOptions::default()),
        faux_assistant_message("never", FauxMessageOptions::default()),
    ]);
    let models = faux.provider();
    let model = faux.get_model();
    let deferred = SimpleStreamOptions { deferred: Some(DeferredOption::Enabled(true)), ..Default::default() };
    let submitted = models.complete_simple(&model, &hi(), deferred.clone()).await;
    assert_eq!(submitted.stop_reason, StopReason::Deferred);
    let handle = submitted.deferred.clone().unwrap();
    assert_eq!(
        (handle.provider.as_str(), handle.model_id.as_str(), handle.poll_after_ms),
        ("faux", "faux-1", Some(250))
    );
    let pending = models.fetch_deferred(&model, &handle, DeferredFetchOptions::default()).await;
    assert_eq!(pending.stop_reason, StopReason::Deferred);
    let ready = models.fetch_deferred(&model, &handle, DeferredFetchOptions::default()).await;
    assert_eq!((ready.stop_reason, ready.text()), (StopReason::Stop, "later".to_string()));
    assert_eq!(models.fetch_deferred(&model, &handle, DeferredFetchOptions::default()).await.text(), "later");

    let second = models.complete_simple(&model, &hi(), deferred).await.deferred.unwrap();
    models.cancel_deferred(&model, &second, DeferredFetchOptions::default()).await.unwrap();
    let cancelled = models.fetch_deferred(&model, &second, DeferredFetchOptions::default()).await;
    assert_eq!(cancelled.error_message, Some(format!("Faux deferred response was cancelled: {}", second.id)));
    let state = faux.state();
    assert_eq!((state.call_count, state.deferred_fetch_count, state.cancelled_deferred), (2, 4, vec![second]));
}

#[test]
fn deferred_option_matches_the_upstream_json_shape() {
    let parse = |text: &str| serde_json::from_str::<DeferredOption>(text).unwrap();
    assert_eq!(parse("true"), DeferredOption::Enabled(true));
    assert_eq!(parse(r#"{"window":"1h"}"#).to_stream_option(), Some("1h".into()));
    assert_eq!(parse("{}").to_stream_option(), Some(String::new()));
    assert!(!parse("false").is_requested());
}

#[tokio::test]
async fn registry_models_report_setup_failures_as_error_events() {
    let models = create_models();
    let unknown =
        Model { id: "m".into(), provider: "nowhere".into(), api: "openai-completions".into(), ..Model::default() };
    let message = models.complete_simple(&unknown, &hi(), SimpleStreamOptions::default()).await;
    assert_eq!(message.error_message.as_deref(), Some("Unknown provider: nowhere"));
    assert!(
        models
            .cancel_deferred(
                &unknown,
                &pi_ai::DeferredHandle {
                    provider: "nowhere".into(),
                    model_id: "m".into(),
                    api: "openai-completions".into(),
                    id: "d".into(),
                    expires_at: None,
                    poll_after_ms: None,
                    data: None,
                },
                DeferredFetchOptions::default()
            )
            .await
            .is_err()
    );

    let builtin = RegistryModels::builtin();
    let model = builtin.get_models(None).into_iter().next().expect("bundled models");
    assert_eq!(builtin.get_model(&model.provider, &model.id), Some(model.clone()));
    let options = SimpleStreamOptions { api_key: Some("test-key".into()), ..SimpleStreamOptions::default() };
    let routed = Model { base_url: "http://127.0.0.1:9".into(), ..model };
    let failed = builtin.complete_simple(&routed, &hi(), options).await;
    assert_eq!(failed.stop_reason, StopReason::Error);
}

#[test]
fn uuidv7_ids_are_version_7_and_sort_in_creation_order() {
    let ids: Vec<String> = (0..100).map(|_| uuidv7()).collect();
    for id in &ids {
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "7");
        assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"));
    }
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(sorted, ids);
    assert!(uuidv7_at(0).unwrap().starts_with("00000000-0000-7"));
    assert!(uuidv7_at(0x1_0000_0000_0000).is_err());
}

#[test]
fn validates_and_coerces_tool_arguments() {
    let tool = Tool::new(
        "echo",
        "Echo",
        json!({"type": "object", "properties": {"count": {"type": "number"}}, "required": ["count"]}),
    );
    let call = |arguments: serde_json::Value| ToolCall {
        id: "c".into(),
        name: "echo".into(),
        arguments: arguments.as_object().unwrap().clone(),
        ..ToolCall::default()
    };
    assert_eq!(validate_tool_arguments(&tool, &call(json!({"count": "3"}))).unwrap()["count"], json!(3));
    let error = validate_tool_arguments(&tool, &call(json!({}))).unwrap_err();
    assert!(error.starts_with("Validation failed for tool \"echo\""), "{error}");
}
