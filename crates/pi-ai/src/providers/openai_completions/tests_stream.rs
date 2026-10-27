//! Stream handling, ported from upstream `openai-completions-response-model`, `-raw-stop-reason`,
//! `-provider-stream-event`, `-retry`, `-reasoning-details`, and stream cases of `-tool-choice`.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::OpenAICompletions;
use super::test_support::{API, done, options, user_context};
use crate::providers::mock_server::{MockResponse, MockServer, collect, frame, kinds, test_model};
use crate::registry::{ApiProvider, StreamOptions};
use crate::stream::AssistantMessageEvent;
use crate::types::{AssistantMessage, ContentBlock, Model, StopReason};

fn data(chunks: &[Value]) -> Vec<String> {
    chunks.iter().map(|chunk| frame(None, chunk)).chain([done()]).collect()
}

fn choice(id: &str, delta: Value, finish_reason: Value) -> Value {
    json!({"id": id, "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]})
}

async fn run_with(
    model: &Model,
    chunks: &[Value],
    options: StreamOptions,
) -> (Vec<AssistantMessageEvent>, AssistantMessage) {
    let server = MockServer::start(MockResponse::Sse(data(chunks))).await;
    let model = Model { base_url: server.base_url.clone(), ..model.clone() };
    collect(OpenAICompletions.stream(&model, &user_context("hi"), options)).await
}

async fn run(chunks: &[Value]) -> (Vec<AssistantMessageEvent>, AssistantMessage) {
    run_with(&test_model(API, "openai", ""), chunks, options()).await
}

fn usage(prompt: u64, completion: u64) -> Value {
    json!({"prompt_tokens": prompt, "completion_tokens": completion,
        "prompt_tokens_details": {"cached_tokens": 0}, "completion_tokens_details": {"reasoning_tokens": 0}})
}

#[tokio::test]
async fn routed_chunk_models_become_the_response_model() {
    let mut router = test_model(API, "openrouter", "");
    router.id = "openrouter/auto".into();
    let routed = [
        json!({"id": "c1", "model": "anthropic/claude-opus-4.8", "choices": [{"index": 0, "delta": {"content": "hi"}}]}),
        json!({"id": "c1", "model": "anthropic/claude-opus-4.8", "choices": [{"index": 0, "delta": {},
            "finish_reason": "stop"}], "usage": usage(10, 5)}),
    ];
    let (_, message) = run_with(&router, &routed, options()).await;
    assert_eq!((message.model.as_str(), message.provider.as_str()), ("openrouter/auto", "openrouter"));
    assert_eq!(message.response_model.as_deref(), Some("anthropic/claude-opus-4.8"));
    assert_eq!(message.stop_reason, StopReason::Stop);
    let echoed = [json!({"id": "c2", "model": "openrouter/auto", "choices": [{"index": 0, "delta": {"content": "hi"},
        "finish_reason": "stop"}]})];
    assert_eq!(run_with(&router, &echoed, options()).await.1.response_model, None);
    let empty = [
        json!({"id": "c3", "model": "", "choices": [{"index": 0, "delta": {"content": "!"}}]}),
        choice("c3", json!({}), json!("stop")),
    ];
    assert_eq!(run_with(&router, &empty, options()).await.1.response_model, None);
}

// Regression test for upstream #9980.
#[tokio::test]
async fn openrouter_reported_totals_replace_the_catalog_total() {
    let billed = |usage: Value| {
        [json!({"id": "c", "model": "openrouter/auto",
            "choices": [{"index": 0, "delta": {"content": "done"}, "finish_reason": "stop"}], "usage": usage})]
    };
    let mut router = test_model(API, "openrouter", "");
    router.id = "openrouter/auto".into();
    let non_byok = json!({"prompt_tokens": 100, "completion_tokens": 20, "cost": 0.004, "is_byok": false,
        "cost_details": {"upstream_inference_cost": 0.003}});
    let cost = run_with(&router, &billed(non_byok.clone()), options()).await.1.usage.cost;
    assert_eq!((cost.total, cost.total_from_provider), (0.004, Some(true)), "no double-counted upstream cost");
    assert!((cost.input - 100.0 / 1e6).abs() < 1e-12 && (cost.output - 40.0 / 1e6).abs() < 1e-12);
    let byok = json!({"prompt_tokens": 100, "completion_tokens": 20, "cost": 0.0002, "is_byok": true,
        "cost_details": {"upstream_inference_cost": 0.003}});
    let cost = run_with(&router, &billed(byok), options()).await.1.usage.cost;
    assert!((cost.total - 0.0032).abs() < 1e-10);
    assert_eq!(cost.total_from_provider, Some(true));
    let cost = run_with(&test_model(API, "openai", ""), &billed(non_byok), options()).await.1.usage.cost;
    assert_eq!(cost.total_from_provider, Some(false), "other providers keep the catalog total");
}

#[tokio::test]
async fn raw_finish_reasons_are_kept_and_unknown_ones_fail() {
    let (_, message) = run(&[choice("c1", json!({}), json!("stop"))]).await;
    assert_eq!((message.stop_reason, message.raw_stop_reason.as_deref()), (StopReason::Stop, Some("stop")));
    assert_eq!(message.error_message, None);
    for reason in ["content_filter", "network_error"] {
        let chunks = [choice("c2", json!({"content": "partial"}), Value::Null), choice("c2", json!({}), json!(reason))];
        let (_, message) = run(&chunks).await;
        assert_eq!((message.stop_reason, message.raw_stop_reason.as_deref()), (StopReason::Error, Some(reason)));
        assert_eq!(message.error_message, Some(format!("Provider finish_reason: {reason}")));
    }
}

#[tokio::test]
async fn null_chunks_are_ignored_and_missing_finish_reasons_fail() {
    let chunks = [
        Value::Null,
        choice("chatcmpl-test", json!({"content": "OK"}), Value::Null),
        json!({"id": "chatcmpl-test", "choices": [{"delta": {}, "finish_reason": "stop"}], "usage": usage(3, 1)}),
    ];
    let (_, message) = run(&chunks).await;
    assert_eq!((message.stop_reason, message.error_message.clone()), (StopReason::Stop, None));
    assert_eq!((message.response_id.as_deref(), message.usage.total_tokens), (Some("chatcmpl-test"), 4));
    assert_eq!(message.content, [ContentBlock::text("OK")]);
    let partial = choice("chatcmpl-truncated", json!({"content": "partial answer"}), Value::Null);
    let (_, message) = run(&[partial.clone(), partial.clone()]).await;
    assert_eq!(message.error_message.as_deref(), Some("Stream ended without finish_reason"));
    let mut model = test_model(API, "openai", "");
    model.compat = Some(json!({"supportsFinishReason": false}));
    let (_, message) =
        run_with(&model, &[choice("c", json!({"content": "complete answer"}), Value::Null)], options()).await;
    assert_eq!((message.stop_reason, message.text()), (StopReason::Stop, "complete answer".to_string()));
}

#[tokio::test]
async fn usage_counts_cache_reads_writes_and_reasoning() {
    let reasoning = json!({"prompt_tokens": 10, "completion_tokens": 33,
        "prompt_tokens_details": {"cached_tokens": 0}, "completion_tokens_details": {"reasoning_tokens": 21}});
    let (_, message) =
        run(&[json!({"id": "c", "choices": [{"delta": {}, "finish_reason": "stop"}], "usage": reasoning})]).await;
    let usage = &message.usage;
    assert_eq!((usage.input, usage.output, usage.total_tokens, usage.reasoning), (10, 33, 43, Some(21)));
    let cached = json!({"prompt_tokens": 100, "completion_tokens": 5,
        "prompt_tokens_details": {"cached_tokens": 50, "cache_write_tokens": 30}});
    let on_chunk = [
        choice("c", json!({"content": "OK"}), Value::Null),
        json!({"id": "c", "choices": [{"delta": {}, "finish_reason": "stop"}], "usage": cached}),
    ];
    let on_choice = [
        choice("c", json!({"content": "OK"}), Value::Null),
        json!({"id": "c", "choices": [{"delta": {}, "finish_reason": "stop", "usage": cached}]}),
    ];
    for chunks in [on_chunk, on_choice] {
        let usage = run(&chunks).await.1.usage;
        assert_eq!((usage.input, usage.cache_read, usage.cache_write, usage.total_tokens), (20, 50, 30, 105));
        assert_eq!(usage.reasoning, Some(0), "absent reasoning tokens count as zero");
        assert!((usage.cost.total - (20.0 + 50.0 * 0.5 + 30.0 * 4.0 + 5.0 * 2.0) / 1e6).abs() < 1e-12);
    }
}

fn call(index: Option<u64>, id: Option<&str>, name: Option<&str>, arguments: &str) -> Value {
    let mut call = json!({"type": "function", "function": {"name": name, "arguments": arguments}});
    if let Some(index) = index {
        call["index"] = json!(index);
    }
    if let Some(id) = id {
        call["id"] = json!(id);
    }
    call
}

fn tool_events(events: &[AssistantMessageEvent], index: usize) -> Vec<String> {
    let indexed = events.iter().filter(|event| match event {
        AssistantMessageEvent::ToolcallStart { content_index, .. }
        | AssistantMessageEvent::ToolcallDelta { content_index, .. }
        | AssistantMessageEvent::ToolcallEnd { content_index, .. } => *content_index == index,
        _ => false,
    });
    kinds(&indexed.cloned().collect::<Vec<_>>())
}

#[tokio::test]
async fn tool_call_deltas_coalesce_by_stable_index_when_ids_change() {
    let chunks = [
        choice("k", json!({"tool_calls": [call(Some(0), Some("functions.read:0"), Some("read"), "")]}), Value::Null),
        choice(
            "k",
            json!({"tool_calls": [call(Some(0), Some("chatcmpl-tool-a"), None, "{\"path\":\"README")]}),
            Value::Null,
        ),
        choice(
            "k",
            json!({"tool_calls": [call(Some(0), Some("chatcmpl-tool-b"), None, ".md\"}")]}),
            json!("tool_calls"),
        ),
    ];
    let (events, message) = run(&chunks).await;
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(
        tool_events(&events, 0),
        ["toolcall_start", "toolcall_delta", "toolcall_delta", "toolcall_delta", "toolcall_end"]
    );
    let calls: Vec<_> = message.tool_calls().collect();
    assert_eq!((calls.len(), calls[0].id.as_str(), calls[0].name.as_str()), (1, "functions.read:0", "read"));
    assert_eq!(Value::Object(calls[0].arguments.clone()), json!({"path": "README.md"}));
}

#[tokio::test]
async fn mixed_content_reasoning_and_parallel_tool_deltas_accumulate_independently() {
    let first = json!({"content": "answer 1", "reasoning_content": "think 1", "tool_calls": [
        call(Some(0), Some("tc_read_initial"), Some("read"), "{\"path\":\"README"),
        call(Some(1), Some("tc_grep_initial"), Some("grep"), "{\"pattern\":\"TODO"),
        call(None, Some("tc_list_no_index"), Some("list"), "{\"path\":\"packages"),
        call(None, Some("tc_write_no_index"), Some("write"), "{\"path\":\"out"),
    ]});
    let second = json!({"content": " answer 2", "tool_calls": [
        call(Some(1), Some("tc_grep_changed"), None, "\",\"path\":\"src"),
        call(None, Some("tc_write_no_index"), None, ".txt\",\"content\":\"ok\"}"),
        call(None, Some("tc_list_no_index"), None, "/ai\"}"),
    ]});
    let third = json!({"content": "\n", "reasoning_content": " think 2", "tool_calls": [
        call(Some(0), Some("tc_read_changed"), None, ".md\"}"),
        call(Some(1), None, None, "\"}"),
    ]});
    let chunks =
        [choice("m", first, Value::Null), choice("m", second, Value::Null), choice("m", third, json!("tool_calls"))];
    let (events, message) = run(&chunks).await;
    let count = |kind: &str| kinds(&events).iter().filter(|event| *event == kind).count();
    let counts: Vec<usize> =
        ["text_start", "text_delta", "text_end", "thinking_start", "thinking_delta", "thinking_end"]
            .into_iter()
            .map(count)
            .collect();
    assert_eq!(counts, [1, 3, 1, 1, 2, 1]);
    assert_eq!((count("toolcall_start"), count("toolcall_delta"), count("toolcall_end")), (4, 9, 4));
    assert_eq!(
        tool_events(&events, 3),
        ["toolcall_start", "toolcall_delta", "toolcall_delta", "toolcall_delta", "toolcall_end"]
    );
    assert_eq!(message.content.len(), 6);
    let thinking = ContentBlock::Thinking {
        thinking: "think 1 think 2".into(),
        thinking_signature: Some("reasoning_content".into()),
        redacted: false,
    };
    assert_eq!(message.content[0], thinking, "reasoning in the same delta as content comes first");
    assert_eq!(message.content[1], ContentBlock::text("answer 1 answer 2\n"));
    let calls: Vec<_> =
        message.tool_calls().map(|call| (call.id.clone(), Value::Object(call.arguments.clone()))).collect();
    assert_eq!(
        calls,
        [
            ("tc_read_initial".to_string(), json!({"path": "README.md"})),
            ("tc_grep_initial".to_string(), json!({"pattern": "TODO", "path": "src"})),
            ("tc_list_no_index".to_string(), json!({"path": "packages/ai"})),
            ("tc_write_no_index".to_string(), json!({"path": "out.txt", "content": "ok"})),
        ]
    );
}

#[tokio::test]
async fn empty_custom_objects_on_function_calls_are_ignored() {
    let mut read = call(Some(0), Some("call_1"), Some("read"), "{\"path\":\"README.md\"}");
    read["custom"] = json!({});
    let (_, message) = run(&[choice("c", json!({"tool_calls": [read]}), json!("tool_calls"))]).await;
    let calls: Vec<_> = message.tool_calls().collect();
    assert_eq!((calls[0].id.as_str(), calls[0].name.as_str()), ("call_1", "read"));
    assert_eq!(Value::Object(calls[0].arguments.clone()), json!({"path": "README.md"}));
}

#[tokio::test]
async fn opencode_go_reasoning_is_stored_as_reasoning_content() {
    let chunks = [choice("c", json!({"reasoning": "think"}), json!("stop"))];
    let signature = |message: &AssistantMessage| match &message.content[0] {
        ContentBlock::Thinking { thinking_signature, .. } => thinking_signature.clone(),
        _ => None,
    };
    let go = test_model(API, "opencode-go", "");
    assert_eq!(signature(&run_with(&go, &chunks, options()).await.1).as_deref(), Some("reasoning_content"));
    assert_eq!(signature(&run(&chunks).await.1).as_deref(), Some("reasoning"));
}

#[tokio::test]
async fn reasoning_details_are_merged_into_the_thinking_signature() {
    let text = |text: &str, extra: Value| {
        let mut detail = json!({"type": "reasoning.text", "text": text, "format": "unknown", "index": 0});
        detail.as_object_mut().unwrap().extend(extra.as_object().cloned().unwrap());
        detail
    };
    let encrypted = json!({"type": "reasoning.encrypted", "id": "rs_1", "data": "opaque", "format": "x", "index": 1});
    let summary = json!({"type": "reasoning.summary", "summary": "Sum", "index": 2});
    let chunks = [
        choice("r", json!({"reasoning": "Let me", "reasoning_details": [text("Let me", json!({}))]}), Value::Null),
        choice(
            "r",
            json!({"reasoning_details": [text(" think", json!({"signature": "sig"})), {"type": "bogus"}]}),
            Value::Null,
        ),
        choice("r", json!({"reasoning_details": [encrypted.clone(), summary.clone()]}), Value::Null),
        choice("r", json!({"content": "Done"}), json!("stop")),
    ];
    let (_, message) = run(&chunks).await;
    let ContentBlock::Thinking { thinking, thinking_signature, .. } = &message.content[0] else { panic!("thinking") };
    assert_eq!(message.content[1], ContentBlock::text("Done"));
    assert_eq!(thinking, "Let me");
    let expected = json!([text("Let me think", json!({"signature": "sig"})), encrypted, summary]);
    assert_eq!(serde_json::from_str::<Value>(thinking_signature.as_deref().unwrap()).unwrap(), expected);
}

#[tokio::test]
async fn provider_chunks_reach_the_stream_event_hook() {
    let first = json!({"id": "c1", "model": "anthropic/claude-sonnet-4.6", "choices": [{"index": 0, "delta": {"content": "hello"}}]});
    let last = json!({"id": "c1", "model": "anthropic/claude-sonnet-4.6",
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12, "cost": 0.0012, "is_byok": false},
        "openrouter_metadata": {"strategy": "direct", "region": "iad"}});
    let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
    let sink = seen.clone();
    let options = StreamOptions {
        on_provider_stream_event: Some(Arc::new(move |data, _| sink.lock().unwrap().push(data.clone()))),
        ..options()
    };
    let (_, message) = run_with(&test_model(API, "openrouter", ""), &[first.clone(), last.clone()], options).await;
    assert_eq!(message.content, [ContentBlock::text("hello")]);
    assert_eq!(*seen.lock().unwrap(), [first, last]);
}

#[tokio::test]
async fn in_stream_errors_include_raw_provider_metadata() {
    let error = json!({"error": {"message": "Provider returned error", "code": 502, "metadata": {"raw": "upstream overloaded"}}});
    let (_, message) = run(&[choice("c", json!({"content": "partial"}), Value::Null), error]).await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(message.error_message.as_deref(), Some("Provider returned error\nupstream overloaded"));
}

#[tokio::test]
async fn retries_honor_server_delays_and_fail_fast_beyond_the_limit() {
    let busy = |status: u16, message: &str| {
        let body = json!({"error": {"message": message}}).to_string();
        MockResponse::with_headers(status, &[("retry-after-ms", "10")], body)
    };
    let ok = MockResponse::Sse(data(&[choice("c", json!({"content": "ok"}), json!("stop"))]));
    let server = MockServer::sequence(vec![busy(429, "rate limited"), busy(500, "server error"), ok]).await;
    let model = test_model(API, "opencode-go", &server.base_url);
    let options = StreamOptions { max_retries: Some(2), max_retry_delay_ms: Some(100), ..options() };
    let (_, message) = collect(OpenAICompletions.stream(&model, &user_context("hi"), options)).await;
    assert_eq!((message.stop_reason, message.text()), (StopReason::Stop, "ok".to_string()));
    assert_eq!(server.requests().len(), 3);
    let long = MockResponse::with_headers(429, &[("retry-after", "277403")], r#"{"error":{"message":"rate limited"}}"#);
    let server = MockServer::sequence(vec![long]).await;
    let model = test_model(API, "opencode-go", &server.base_url);
    let options = StreamOptions { max_retries: Some(2), max_retry_delay_ms: Some(1000), ..self::options() };
    let (_, message) = collect(OpenAICompletions.stream(&model, &user_context("hi"), options)).await;
    let error = message.error_message.unwrap();
    assert!(
        error.contains("Server requested 277403s retry delay (max: 1s)") && error.contains("rate limited"),
        "{error}"
    );
    assert_eq!(server.requests().len(), 1);
}
