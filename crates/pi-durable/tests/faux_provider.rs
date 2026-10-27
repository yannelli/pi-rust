//! Port of upstream `packages/ai/test/faux-provider.test.ts`. Upstream drives the faux provider through the global
//! `stream`/`complete` compat API; here requests go through the [`Models`] trait of `handle.provider()`.

use futures::StreamExt;
use pi_ai::{
    AssistantMessage, AssistantMessageEvent, CacheRetention, ContentBlock, Context, Message, StopReason, Tool,
    ToolResultMessage, UserContent, UserMessage, now_ms,
};
use pi_chord::AbortController;
use pi_durable::models::{Models, SimpleStreamOptions, create_models};
use pi_durable::testing::faux::{
    FauxMessageOptions, FauxModelDefinition, FauxProviderHandle, FauxResponseStep, FauxTokenSize,
    RegisterFauxProviderOptions, faux_assistant_message, faux_provider, faux_text, faux_thinking, faux_tool_call,
    faux_tool_call_with_id,
};
use serde_json::json;

fn user(text: &str) -> Message {
    Message::User(UserMessage { content: UserContent::Text(text.into()), timestamp: now_ms() })
}

fn hi() -> Context {
    Context { messages: vec![user("hi")], ..Context::default() }
}

fn message(content: &str) -> AssistantMessage {
    faux_assistant_message(content, FauxMessageOptions::default())
}

async fn complete(faux: &FauxProviderHandle, context: &Context, options: SimpleStreamOptions) -> AssistantMessage {
    faux.provider().complete_simple(&faux.get_model(), context, options).await
}

async fn collect(
    faux: &FauxProviderHandle,
    context: &Context,
    options: SimpleStreamOptions,
) -> Vec<AssistantMessageEvent> {
    faux.provider().stream_simple(&faux.get_model(), context, options).collect().await
}

fn kind(event: &AssistantMessageEvent) -> String {
    serde_json::to_value(event).unwrap()["type"].as_str().unwrap().to_string()
}

fn session(id: &str) -> SimpleStreamOptions {
    SimpleStreamOptions {
        session_id: Some(id.into()),
        cache_retention: Some(CacheRetention::Short),
        ..SimpleStreamOptions::default()
    }
}

fn paced(tokens_per_second: f64) -> RegisterFauxProviderOptions {
    RegisterFauxProviderOptions {
        tokens_per_second: Some(tokens_per_second),
        token_size: Some(FauxTokenSize { min: Some(3), max: Some(3) }),
        ..RegisterFauxProviderOptions::default()
    }
}

fn token_size(size: u32) -> RegisterFauxProviderOptions {
    RegisterFauxProviderOptions {
        token_size: Some(FauxTokenSize { min: Some(size), max: Some(size) }),
        ..RegisterFauxProviderOptions::default()
    }
}

#[tokio::test]
async fn registers_a_custom_provider_and_estimates_usage() {
    let faux = faux_provider(RegisterFauxProviderOptions::default());
    faux.set_responses([message("hello world")]);
    let context =
        Context { system_prompt: Some("Be concise.".into()), messages: vec![user("hi there")], tools: vec![] };
    let response = complete(&faux, &context, SimpleStreamOptions::default()).await;
    assert_eq!(response.content, vec![faux_text("hello world")]);
    assert!(response.usage.input > 0);
    assert!(response.usage.output > 0);
    assert_eq!(response.usage.total_tokens, response.usage.input + response.usage.output);
    assert_eq!(faux.state().call_count, 1);
}

#[tokio::test]
async fn supports_helper_blocks_for_text_thinking_and_tool_calls() {
    let faux = faux_provider(RegisterFauxProviderOptions::default());
    faux.set_responses([faux_assistant_message(
        vec![faux_thinking("think"), faux_tool_call("echo", json!({"text": "hi"})), faux_text("done")],
        FauxMessageOptions::stop_reason(StopReason::ToolUse),
    )]);
    let response = complete(&faux, &hi(), SimpleStreamOptions::default()).await;
    assert_eq!(response.content.len(), 3);
    assert_eq!(response.content[0], faux_thinking("think"));
    let ContentBlock::ToolCall(call) = &response.content[1] else { panic!("expected a tool call") };
    assert!(!call.id.is_empty());
    assert_eq!((call.name.as_str(), &call.arguments), ("echo", json!({"text": "hi"}).as_object().unwrap()));
    assert_eq!(response.content[2], faux_text("done"));
    assert_eq!(response.stop_reason, StopReason::ToolUse);
}

#[tokio::test]
async fn supports_multiple_models_with_per_model_reasoning_and_model_aware_factories() {
    let faux = faux_provider(RegisterFauxProviderOptions {
        models: vec![
            FauxModelDefinition {
                name: Some("Faux Fast".into()),
                reasoning: Some(false),
                ..FauxModelDefinition::new("faux-fast")
            },
            FauxModelDefinition {
                name: Some("Faux Thinker".into()),
                reasoning: Some(true),
                ..FauxModelDefinition::new("faux-thinker")
            },
        ],
        ..RegisterFauxProviderOptions::default()
    });
    let step = || FauxResponseStep::factory(|_, _, _, model| message(&format!("{}:{}", model.id, model.reasoning)));
    faux.set_responses([step(), step()]);
    let ids: Vec<&str> = faux.models.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(ids, ["faux-fast", "faux-thinker"]);
    assert_eq!(faux.get_model(), faux.models[0]);
    assert!(!faux.get_model_by_id("faux-fast").unwrap().reasoning);
    assert!(faux.get_model_by_id("faux-thinker").unwrap().reasoning);
    let models = faux.provider();
    let fast = models.complete_simple(&faux.get_model_by_id("faux-fast").unwrap(), &hi(), Default::default()).await;
    let thinker =
        models.complete_simple(&faux.get_model_by_id("faux-thinker").unwrap(), &hi(), Default::default()).await;
    assert_eq!(fast.content, vec![faux_text("faux-fast:false")]);
    assert_eq!(thinker.content, vec![faux_text("faux-thinker:true")]);
}

#[tokio::test]
async fn rewrites_api_provider_and_model_on_returned_messages() {
    let faux = faux_provider(RegisterFauxProviderOptions {
        api: Some("faux:test".into()),
        provider: Some("faux-provider".into()),
        models: vec![FauxModelDefinition::new("faux-model")],
        ..RegisterFauxProviderOptions::default()
    });
    faux.set_responses([message("hello")]);
    let response = complete(&faux, &hi(), SimpleStreamOptions::default()).await;
    assert_eq!((response.api.as_str(), response.provider.as_str()), ("faux:test", "faux-provider"));
    assert_eq!(response.model, "faux-model");
}

#[tokio::test]
async fn consumes_queued_responses_in_order_and_errors_when_exhausted() {
    let faux = faux_provider(RegisterFauxProviderOptions::default());
    faux.set_responses([message("first"), message("second")]);
    let first = complete(&faux, &hi(), SimpleStreamOptions::default()).await;
    let second = complete(&faux, &hi(), SimpleStreamOptions::default()).await;
    let exhausted = complete(&faux, &hi(), SimpleStreamOptions::default()).await;
    assert_eq!(first.content, vec![faux_text("first")]);
    assert_eq!(second.content, vec![faux_text("second")]);
    assert_eq!(exhausted.stop_reason, StopReason::Error);
    assert_eq!(exhausted.error_message.as_deref(), Some("No more faux responses queued"));
    assert_eq!(faux.get_pending_response_count(), 0);
    assert_eq!(faux.state().call_count, 3);
}

#[tokio::test]
async fn can_replace_and_append_queued_responses() {
    let faux = faux_provider(RegisterFauxProviderOptions::default());
    faux.set_responses([message("first")]);
    let text = |message: AssistantMessage| message.text();
    assert_eq!(text(complete(&faux, &hi(), Default::default()).await), "first");
    assert_eq!(faux.get_pending_response_count(), 0);
    faux.set_responses([message("second")]);
    assert_eq!(faux.get_pending_response_count(), 1);
    assert_eq!(text(complete(&faux, &hi(), Default::default()).await), "second");
    faux.append_responses([message("third"), message("fourth")]);
    assert_eq!(faux.get_pending_response_count(), 2);
    assert_eq!(text(complete(&faux, &hi(), Default::default()).await), "third");
    assert_eq!(text(complete(&faux, &hi(), Default::default()).await), "fourth");
    assert_eq!(faux.get_pending_response_count(), 0);
}

#[tokio::test]
async fn supports_async_response_factories() {
    let faux = faux_provider(RegisterFauxProviderOptions::default());
    faux.set_responses([FauxResponseStep::async_factory(|context, _, state, _| {
        let text = format!("{}:{}", context.messages.len(), state.call_count);
        Box::pin(async move { Ok(message(&text)) })
    })]);
    let response = complete(&faux, &hi(), SimpleStreamOptions::default()).await;
    assert_eq!(response.content, vec![faux_text("1:1")]);
}

#[tokio::test]
async fn emits_an_error_when_a_response_factory_throws() {
    let faux = faux_provider(RegisterFauxProviderOptions::default());
    faux.set_responses([FauxResponseStep::try_factory(|_, _, _, _| Err("boom".into()))]);
    let events = collect(&faux, &hi(), SimpleStreamOptions::default()).await;
    assert_eq!(events.len(), 1);
    let AssistantMessageEvent::Error { error, .. } = &events[0] else { panic!("expected an error event") };
    assert_eq!(error.stop_reason, StopReason::Error);
    assert_eq!(error.error_message.as_deref(), Some("boom"));
}

#[tokio::test]
async fn rejects_a_queued_response_without_a_terminal_stop_reason() {
    let faux = faux_provider(RegisterFauxProviderOptions::default());
    faux.set_responses([faux_assistant_message("partial", FauxMessageOptions::stop_reason(StopReason::Pending))]);
    let events = collect(&faux, &hi(), SimpleStreamOptions::default()).await;
    assert!(!events.iter().any(|event| matches!(event, AssistantMessageEvent::Done { .. })));
    let Some(AssistantMessageEvent::Error { error, .. }) = events.last() else { panic!("expected an error event") };
    assert_eq!(error.stop_reason, StopReason::Error);
    assert_eq!(error.error_message.as_deref(), Some("Faux response ended without a stop reason"));
}

#[tokio::test]
async fn estimates_prompt_and_output_tokens_from_serialized_context() {
    let faux = faux_provider(RegisterFauxProviderOptions::default());
    faux.set_responses([message("done")]);
    let schema = json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]});
    let tool = Tool::new("echo", "Echo back text", schema);
    let image = ContentBlock::Image { data: "abcd".into(), mime_type: "image/png".into() };
    let context = Context {
        system_prompt: Some("sys".into()),
        messages: vec![
            Message::User(UserMessage { content: UserContent::Blocks(vec![faux_text("hello"), image]), timestamp: 1 }),
            Message::Assistant(message("prior")),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "tool-1".into(),
                tool_name: "echo".into(),
                content: vec![faux_text("tool out")],
                is_error: false,
                timestamp: 2,
                ..ToolResultMessage::default()
            }),
        ],
        tools: vec![tool.clone()],
    };
    let response = complete(&faux, &context, SimpleStreamOptions::default()).await;
    // The leading system message carries the tools, as the faux provider renders a normalized transcript.
    let prompt_text = [
        format!("system:sys\ntool+:{}", serde_json::to_string(&tool).unwrap()),
        "user:hello\n[image:image/png:4]".into(),
        "assistant:prior".into(),
        "toolResult:echo\ntool out".into(),
    ]
    .join("\n\n");
    let expected_prompt_tokens = prompt_text.len().div_ceil(4) as u64;
    let expected_output_tokens = "done".len().div_ceil(4) as u64;
    assert_eq!(response.usage.input, expected_prompt_tokens);
    assert_eq!(response.usage.output, expected_output_tokens);
    assert_eq!((response.usage.cache_read, response.usage.cache_write), (0, 0));
    assert_eq!(response.usage.total_tokens, expected_prompt_tokens + expected_output_tokens);
}

#[tokio::test]
async fn does_not_share_cache_across_sessions_or_requests_without_session_id() {
    let faux = faux_provider(RegisterFauxProviderOptions::default());
    faux.set_responses([message("first"), message("second"), message("third")]);
    let mut context = Context { messages: vec![user("hello")], ..Context::default() };
    let first = complete(&faux, &context, session("session-1")).await;
    assert!(first.usage.cache_write > 0);
    context.messages.push(Message::Assistant(first));
    context.messages.push(user("follow up"));
    let second = complete(&faux, &context, session("session-2")).await;
    assert_eq!(second.usage.cache_read, 0);
    assert!(second.usage.cache_write > 0);
    let third = complete(&faux, &context, SimpleStreamOptions::default()).await;
    assert_eq!((third.usage.cache_read, third.usage.cache_write), (0, 0));
}

#[tokio::test]
async fn simulates_prompt_caching_per_session_id() {
    let faux = faux_provider(RegisterFauxProviderOptions::default());
    faux.set_responses([message("first"), message("second")]);
    let mut context =
        Context { system_prompt: Some("Be concise.".into()), messages: vec![user("hello")], tools: vec![] };
    let first = complete(&faux, &context, session("session-1")).await;
    assert_eq!(first.usage.cache_read, 0);
    assert!(first.usage.cache_write > 0);
    context.messages.push(Message::Assistant(first));
    context.messages.push(user("follow up"));
    let second = complete(&faux, &context, session("session-1")).await;
    assert!(second.usage.cache_read > 0);
    assert!(second.usage.input + second.usage.cache_read > second.usage.input);
}

#[tokio::test]
async fn counts_cached_characters_up_to_the_first_difference_in_the_joined_prompt() {
    let faux = faux_provider(RegisterFauxProviderOptions::default());
    faux.set_responses([message("a"), message("b"), message("c")]);
    let user_at = |text: &str| Message::User(UserMessage { content: UserContent::Text(text.into()), timestamp: 1 });
    let context = |messages: Vec<Message>| Context { messages, ..Context::default() };
    // Prompt texts: "user:hello world" (16 characters), then 16 + 2 + "user:next" (9) = 27.
    let first = complete(&faux, &context(vec![user_at("hello world")]), session("session-1")).await;
    let usage = |message: &AssistantMessage| (message.usage.input, message.usage.cache_read, message.usage.cache_write);
    assert_eq!(usage(&first), (0, 0, 4));
    let extended = complete(&faux, &context(vec![user_at("hello world"), user_at("next")]), session("session-1")).await;
    assert_eq!(usage(&extended), (0, 4, 3));
    // The first message now differs after "user:hello w" (12 characters): "user:hello wide" + 2 + 9 = 26.
    let edited = complete(&faux, &context(vec![user_at("hello wide"), user_at("next")]), session("session-1")).await;
    assert_eq!(usage(&edited), (0, 3, 4));
}

#[tokio::test]
async fn does_not_simulate_caching_when_cache_retention_is_none() {
    let faux = faux_provider(RegisterFauxProviderOptions::default());
    faux.set_responses([message("first"), message("second")]);
    let mut context = Context { messages: vec![user("hello")], ..Context::default() };
    let options = SimpleStreamOptions {
        session_id: Some("session-1".into()),
        cache_retention: Some(CacheRetention::None),
        ..SimpleStreamOptions::default()
    };
    complete(&faux, &context, options.clone()).await;
    context.messages.push(Message::Assistant(message("first")));
    context.messages.push(user("follow up"));
    let second = complete(&faux, &context, options).await;
    assert_eq!((second.usage.cache_read, second.usage.cache_write), (0, 0));
}

#[path = "faux_provider/streaming.rs"]
mod streaming;
