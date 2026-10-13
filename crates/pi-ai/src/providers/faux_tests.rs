use serde_json::json;

use super::*;
use crate::registry::register_api_provider;
use crate::types::Message;

#[tokio::test]
async fn streams_scripted_text_and_tool_calls() {
    let provider = FauxProvider::new([
        FauxResponse::text("Hello from the faux provider"),
        FauxResponse::tool_call("c1", "read", json!({"path": "README.md"})),
    ]);
    register_api_provider("faux-test", provider.clone());
    let mut model = FauxProvider::model();
    model.api = "faux-test".into();
    let context = Context { messages: vec![Message::user("hi")], ..Context::default() };
    let first = crate::complete(&model, &context, StreamOptions::default()).await;
    assert_eq!(first.text(), "Hello from the faux provider");
    assert_eq!(first.stop_reason, StopReason::Stop);
    let second = crate::complete(&model, &context, StreamOptions::default()).await;
    assert_eq!(second.stop_reason, StopReason::ToolUse);
    assert_eq!(second.tool_calls().next().unwrap().arguments["path"], "README.md");
    assert_eq!(provider.requests().len(), 2);
}

#[tokio::test]
async fn hang_ends_with_abort_when_cancelled() {
    let provider = FauxProvider::new([FauxResponse::Hang]);
    let options = StreamOptions::default();
    let cancel = options.cancel.clone();
    let stream = provider.stream(&FauxProvider::model(), &Context::default(), options);
    cancel.cancel();
    assert_eq!(stream.result(&FauxProvider::model()).await.stop_reason, StopReason::Aborted);
}

#[tokio::test]
async fn deferred_requests_return_handles_until_the_response_is_ready() {
    let provider = FauxProvider::with_deferred([FauxResponse::text("later"), FauxResponse::text("x")], 1, Some(250));
    register_api_provider("faux-deferred", provider.clone());
    let mut model = FauxProvider::model();
    model.api = "faux-deferred".into();
    let context = Context { messages: vec![Message::user("hi")], ..Context::default() };
    let options = StreamOptions { deferred: Some("1h".into()), ..StreamOptions::default() };
    let submitted = crate::complete(&model, &context, options).await;
    assert_eq!(submitted.stop_reason, StopReason::Deferred);
    let handle = submitted.deferred.clone().expect("deferred handle");
    assert_eq!(
        (handle.api.as_str(), handle.model_id.as_str(), handle.poll_after_ms),
        ("faux-deferred", "faux-model", Some(250))
    );
    let encoded = serde_json::to_value(&submitted).unwrap();
    assert_eq!(encoded["stopReason"], "deferred");
    assert_eq!(encoded["deferred"]["modelId"], "faux-model");

    let pending = crate::fetch_deferred(&model, &handle, DeferredOptions::default()).await;
    assert_eq!(pending.stop_reason, StopReason::Deferred);
    let ready = crate::fetch_deferred(&model, &handle, DeferredOptions::default()).await;
    assert_eq!((ready.stop_reason, ready.text()), (StopReason::Stop, "later".to_string()));
    assert_eq!(provider.deferred_fetch_count(), 2);

    let options = StreamOptions { deferred: Some("15m".into()), ..StreamOptions::default() };
    let second = crate::complete(&model, &context, options).await.deferred.unwrap();
    crate::cancel_deferred(&model, &second, DeferredOptions::default()).await.unwrap();
    assert_eq!(provider.cancelled_deferred(), std::slice::from_ref(&second));
    let cancelled = crate::fetch_deferred(&model, &second, DeferredOptions::default()).await;
    assert_eq!(cancelled.error_message, Some(format!("Faux deferred response was cancelled: {}", second.id)));
    let unknown = DeferredHandle { id: "missing".into(), ..second };
    let missing = crate::fetch_deferred(&model, &unknown, DeferredOptions::default()).await;
    assert_eq!(missing.error_message.as_deref(), Some("Unknown faux deferred response: missing"));
}

#[tokio::test]
async fn apis_without_deferred_support_report_errors() {
    let model = crate::providers::mock_server::test_model("openai-completions", "openai", "http://127.0.0.1:9");
    let handle = DeferredHandle {
        provider: "openai".into(),
        model_id: "test-model".into(),
        api: "openai-completions".into(),
        id: "x".into(),
        expires_at: None,
        poll_after_ms: None,
        data: None,
    };
    let message = crate::fetch_deferred(&model, &handle, DeferredOptions::default()).await;
    assert_eq!(message.error_message.as_deref(), Some("API does not support deferred responses"));
    let error = crate::cancel_deferred(&model, &handle, DeferredOptions::default()).await.unwrap_err();
    assert_eq!(error, "API cannot cancel deferred responses");
}
