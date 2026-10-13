//! Streaming, terminal error, abort, and unregister cases of upstream `faux-provider.test.ts`.

use super::*;

/// Stream with `signal` and abort as soon as the first event of `trigger` arrives; returns the event kinds.
async fn abort_on_first(faux: &FauxProviderHandle, trigger: &str) -> (Vec<String>, usize) {
    let controller = AbortController::new();
    let options = SimpleStreamOptions { signal: Some(controller.signal()), ..SimpleStreamOptions::default() };
    let mut stream = faux.provider().stream_simple(&faux.get_model(), &hi(), options);
    let (mut kinds, mut triggered) = (Vec::new(), 0);
    while let Some(event) = stream.next().await {
        let event_kind = kind(&event);
        if event_kind == trigger {
            triggered += 1;
            controller.abort(None);
        }
        kinds.push(event_kind);
    }
    (kinds, triggered)
}

#[tokio::test]
async fn streams_thinking_text_and_partial_tool_call_deltas() {
    let faux = faux_provider(RegisterFauxProviderOptions::default());
    faux.set_responses([faux_assistant_message(
        vec![
            faux_thinking("thinking text"),
            faux_text("answer text"),
            faux_tool_call_with_id("echo", json!({"text": "hi", "count": 12}), "tool-1"),
        ],
        FauxMessageOptions::stop_reason(StopReason::ToolUse),
    )]);
    let events = collect(&faux, &hi(), SimpleStreamOptions::default()).await;
    let kinds: Vec<String> = events.iter().map(kind).collect();
    for expected in [
        "thinking_start",
        "thinking_delta",
        "text_start",
        "text_delta",
        "toolcall_start",
        "toolcall_delta",
        "toolcall_end",
    ] {
        assert!(kinds.iter().any(|kind| kind == expected), "missing {expected}");
    }
    let deltas: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            AssistantMessageEvent::ToolcallDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert!(deltas.len() > 1);
    let joined: serde_json::Value = serde_json::from_str(&deltas.concat()).unwrap();
    assert_eq!(joined, json!({"text": "hi", "count": 12}));
}

#[tokio::test]
async fn streams_an_exact_event_order_for_fixed_size_chunks() {
    let faux = faux_provider(token_size(1));
    faux.set_responses([faux_assistant_message(
        vec![faux_thinking("go"), faux_text("ok"), faux_tool_call_with_id("echo", json!({}), "tool-1")],
        FauxMessageOptions::stop_reason(StopReason::ToolUse),
    )]);
    let events = collect(&faux, &hi(), SimpleStreamOptions::default()).await;
    let AssistantMessageEvent::Start { partial } = &events[0] else { panic!("expected start") };
    assert_eq!(partial.stop_reason, StopReason::Pending);
    let kinds: Vec<String> = events.iter().map(kind).collect();
    assert_eq!(
        kinds,
        [
            "start",
            "thinking_start",
            "thinking_delta",
            "thinking_end",
            "text_start",
            "text_delta",
            "text_end",
            "toolcall_start",
            "toolcall_delta",
            "toolcall_end",
            "done"
        ]
    );
}

#[tokio::test]
async fn streams_multiple_tool_calls_in_one_message() {
    let faux = faux_provider(RegisterFauxProviderOptions::default());
    faux.set_responses([faux_assistant_message(
        vec![
            faux_tool_call_with_id("echo", json!({"text": "one"}), "tool-1"),
            faux_tool_call_with_id("echo", json!({"text": "two"}), "tool-2"),
        ],
        FauxMessageOptions::stop_reason(StopReason::ToolUse),
    )]);
    let kinds: Vec<String> = collect(&faux, &hi(), SimpleStreamOptions::default()).await.iter().map(kind).collect();
    assert_eq!(kinds.iter().filter(|kind| *kind == "toolcall_start").count(), 2);
    assert_eq!(kinds.iter().filter(|kind| *kind == "toolcall_end").count(), 2);
}

async fn assert_terminal_message(stop_reason: StopReason, error_message: &str) {
    let faux = faux_provider(token_size(2));
    faux.set_responses([AssistantMessage {
        stop_reason,
        error_message: Some(error_message.into()),
        ..message("partial")
    }]);
    let events = collect(&faux, &hi(), SimpleStreamOptions::default()).await;
    let kinds: Vec<String> = events.iter().map(kind).collect();
    assert_eq!(kinds, ["start", "text_start", "text_delta", "text_end", "error"]);
    let Some(AssistantMessageEvent::Error { reason, error }) = events.last() else { panic!("expected an error") };
    assert_eq!((*reason, error.stop_reason), (stop_reason, stop_reason));
    assert_eq!(error.error_message.as_deref(), Some(error_message));
}

#[tokio::test]
async fn streams_an_explicit_assistant_error_message_as_a_terminal_error() {
    assert_terminal_message(StopReason::Error, "upstream failed").await;
}

#[tokio::test]
async fn streams_an_explicit_assistant_aborted_message_as_a_terminal_error() {
    assert_terminal_message(StopReason::Aborted, "Request was aborted").await;
}

#[tokio::test]
async fn supports_aborting_before_the_first_chunk() {
    let faux = faux_provider(paced(50.0));
    faux.set_responses([message("abcdefghijklmnopqrstuvwxyz")]);
    let controller = AbortController::new();
    controller.abort(None);
    let options = SimpleStreamOptions { signal: Some(controller.signal()), ..SimpleStreamOptions::default() };
    let events = collect(&faux, &hi(), options).await;
    assert_eq!(events.len(), 1);
    let AssistantMessageEvent::Error { reason, error } = &events[0] else { panic!("expected an error") };
    assert_eq!((*reason, error.stop_reason), (StopReason::Aborted, StopReason::Aborted));
}

#[tokio::test]
async fn supports_aborting_mid_text_stream_when_paced() {
    let faux = faux_provider(paced(100.0));
    faux.set_responses([message("abcdefghijklmnopqrstuvwxyz")]);
    let (kinds, deltas) = abort_on_first(&faux, "text_delta").await;
    assert_eq!(deltas, 1);
    for expected in ["text_start", "text_delta", "error"] {
        assert!(kinds.iter().any(|kind| kind == expected), "missing {expected}");
    }
    assert!(!kinds.iter().any(|kind| kind == "text_end"));
}

#[tokio::test]
async fn supports_aborting_mid_thinking_stream_when_paced() {
    let faux = faux_provider(paced(100.0));
    faux.set_responses([AssistantMessage {
        content: vec![faux_thinking("abcdefghijklmnopqrstuvwxyz")],
        ..message("ignored")
    }]);
    let (kinds, deltas) = abort_on_first(&faux, "thinking_delta").await;
    assert_eq!(deltas, 1);
    for expected in ["thinking_start", "thinking_delta", "error"] {
        assert!(kinds.iter().any(|kind| kind == expected), "missing {expected}");
    }
    assert!(!kinds.iter().any(|kind| kind == "thinking_end"));
}

#[tokio::test]
async fn supports_aborting_mid_toolcall_stream_when_paced() {
    let faux = faux_provider(paced(100.0));
    let call =
        faux_tool_call_with_id("echo", json!({"text": "abcdefghijklmnopqrstuvwxyz", "count": 123456789}), "tool-1");
    faux.set_responses([AssistantMessage { content: vec![call], stop_reason: StopReason::ToolUse, ..message("done") }]);
    let (kinds, deltas) = abort_on_first(&faux, "toolcall_delta").await;
    assert_eq!(deltas, 1);
    for expected in ["toolcall_start", "toolcall_delta", "error"] {
        assert!(kinds.iter().any(|kind| kind == expected), "missing {expected}");
    }
    assert!(!kinds.iter().any(|kind| kind == "toolcall_end"));
}

// Upstream unregisters the API provider from the global registry and expects `complete` to reject. Rust hosts
// compose providers in a `RegistryModels` instead, so removing the provider makes requests fail with an error event.
#[tokio::test]
async fn unregisters_the_provider() {
    let faux = faux_provider(RegisterFauxProviderOptions::default());
    faux.set_responses([message("hello")]);
    let models = create_models();
    models.set_provider(faux.provider_id(), faux.provider());
    assert_eq!(models.get_model("faux", "faux-1"), Some(faux.get_model()));
    models.remove_provider(faux.provider_id());
    assert_eq!(models.get_model("faux", "faux-1"), None);
    let response = models.complete_simple(&faux.get_model(), &hi(), SimpleStreamOptions::default()).await;
    assert_eq!(response.stop_reason, StopReason::Error);
    assert_eq!(response.error_message.as_deref(), Some("Unknown provider: faux"));
}
