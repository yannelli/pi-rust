//! Ported from upstream `test/assistant-message-frame.test.ts`.

use serde_json::json;

use super::*;
use crate::stream::AssistantMessageEvent as E;
use crate::types::Usage;

fn seed() -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: "test-api".into(),
        provider: "test-provider".into(),
        model: "test-model".into(),
        response_model: None,
        response_id: None,
        provider_thinking_level: None,
        thinking_level: None,
        diagnostics: Vec::new(),
        usage: Usage::default(),
        stop_reason: StopReason::Pending,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 1,
        duration_ms: None,
    }
}

fn frame(encoder: &mut AssistantMessageFrameEncoder, event: E) -> AssistantMessageFrame {
    encoder.encode(&event).unwrap().expect("a frame")
}

fn encode_all(events: &[E]) -> Vec<AssistantMessageFrame> {
    let mut encoder = AssistantMessageFrameEncoder::new();
    events.iter().filter_map(|event| encoder.encode(event).unwrap()).collect()
}

fn reduce(frames: &[AssistantMessageFrame]) -> Option<AssistantMessage> {
    reduce_assistant_message_frames(frames).unwrap()
}

fn content_json(frames: &[AssistantMessageFrame]) -> serde_json::Value {
    serde_json::to_value(reduce(frames).unwrap().content).unwrap()
}

fn tool_call(id: &str, name: &str, arguments: serde_json::Value) -> ContentBlock {
    ContentBlock::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: arguments.as_object().unwrap().clone(),
        ..ToolCall::default()
    })
}

fn types(frames: &[AssistantMessageFrame]) -> Vec<String> {
    frames.iter().map(|f| serde_json::to_value(f).unwrap()["type"].as_str().unwrap().to_string()).collect()
}

fn reduce_err(frames: serde_json::Value) -> String {
    let frames: Vec<AssistantMessageFrame> = serde_json::from_value(frames).unwrap();
    reduce_assistant_message_frames(&frames).unwrap_err().0
}

#[test]
fn uses_authoritative_text_end_content_and_signature() {
    let mut partial = seed();
    let mut encoder = AssistantMessageFrameEncoder::new();
    let mut frames = vec![frame(&mut encoder, E::Start { partial: partial.clone() })];
    partial.content.push(ContentBlock::text("Hello "));
    frames.push(frame(&mut encoder, E::TextStart { content_index: 0, partial: partial.clone() }));
    partial.content[0] = ContentBlock::Text { text: "Hello world".into(), text_signature: Some("sig-text".into()) };
    let delta = E::TextDelta { content_index: 0, delta: "incorrect".into(), partial: partial.clone() };
    frames.push(frame(&mut encoder, delta));
    frames.push(frame(&mut encoder, E::TextEnd { content_index: 0, content: "Hello world".into(), partial }));
    assert_eq!(
        serde_json::to_value(frames.last().unwrap()).unwrap(),
        json!({ "type": "text_end", "contentIndex": 0, "content": "Hello world", "textSignature": "sig-text" })
    );
    assert_eq!(content_json(&frames), json!([{ "type": "text", "text": "Hello world", "textSignature": "sig-text" }]));
}

#[test]
fn preserves_provider_thinking_level_from_the_stream_start() {
    let mut partial = seed();
    partial.provider_thinking_level = Some("high".into());
    let start = frame(&mut AssistantMessageFrameEncoder::new(), E::Start { partial });
    assert_eq!(reduce(&[start]).unwrap().provider_thinking_level.as_deref(), Some("high"));
}

#[test]
fn preserves_initial_and_final_thinking_metadata_including_redaction() {
    let thinking = |signature: &str| ContentBlock::Thinking {
        thinking: "[redacted]".into(),
        thinking_signature: Some(signature.into()),
        redacted: true,
    };
    let mut partial = seed();
    let mut encoder = AssistantMessageFrameEncoder::new();
    let mut frames = vec![frame(&mut encoder, E::Start { partial: partial.clone() })];
    partial.content.push(thinking("encrypted-start"));
    frames.push(frame(&mut encoder, E::ThinkingStart { content_index: 0, partial: partial.clone() }));
    partial.content[0] = thinking("encrypted-final");
    frames.push(frame(&mut encoder, E::ThinkingEnd { content_index: 0, content: "[redacted]".into(), partial }));
    let expected = json!({ "type": "thinking_end", "contentIndex": 0, "content": "[redacted]",
        "thinkingSignature": "encrypted-final", "redacted": true });
    assert_eq!(serde_json::to_value(frames.last().unwrap()).unwrap(), expected);
    assert_eq!(reduce(&frames).unwrap().content[0], thinking("encrypted-final"));
}

#[test]
fn parses_unfinished_tool_json_once_and_uses_authoritative_completed_arguments() {
    let initial: Vec<AssistantMessageFrame> = serde_json::from_value(json!([
        { "type": "start", "partial": seed() },
        { "type": "toolcall_start", "contentIndex": 0,
          "toolCall": { "type": "toolCall", "id": "initial-id", "name": "write", "arguments": {} } },
        { "type": "toolcall_delta", "contentIndex": 0, "delta": "{\"path\":\"READ" },
    ]))
    .unwrap();
    assert_eq!(content_json(&initial)[0]["arguments"], json!({ "path": "READ" }));

    let mut complete = initial.clone();
    complete.extend(
        serde_json::from_value::<Vec<AssistantMessageFrame>>(json!([
            { "type": "toolcall_delta", "contentIndex": 0, "delta": "ME.md\",\"lines\":[1,2]}" },
            { "type": "toolcall_end", "contentIndex": 0, "id": "final-id", "name": "write_file",
              "arguments": { "path": "final.md", "lines": [3] }, "thoughtSignature": "thought", "namespace": "files" },
        ]))
        .unwrap(),
    );
    assert_eq!(
        content_json(&complete)[0],
        json!({ "type": "toolCall", "id": "final-id", "name": "write_file",
            "arguments": { "path": "final.md", "lines": [3] }, "thoughtSignature": "thought", "namespace": "files" })
    );
}

#[test]
fn reconciles_queued_text_events_against_one_advanced_live_partial() {
    let mut partial = seed();
    partial.content.push(ContentBlock::text("Hello world"));
    let mut events =
        vec![E::Start { partial: partial.clone() }, E::TextStart { content_index: 0, partial: partial.clone() }];
    for delta in ["Hel", "lo", " ", "world"] {
        events.push(E::TextDelta { content_index: 0, delta: delta.into(), partial: partial.clone() });
    }
    let frames = encode_all(&events);
    assert_eq!(types(&frames), ["start", "text_start"]);
    let AssistantMessageFrame::Start { partial: start } = &frames[0] else { panic!("start frame") };
    assert!(start.content.is_empty() && start.stop_reason == StopReason::Pending);
    assert_eq!(content_json(&frames), json!([{ "type": "text", "text": "Hello world" }]));
}

#[test]
fn trims_only_the_covered_prefix_when_a_start_snapshot_lands_inside_a_delta() {
    let mut partial = seed();
    let mut encoder = AssistantMessageFrameEncoder::new();
    let mut frames = vec![frame(&mut encoder, E::Start { partial: partial.clone() })];
    partial.content.push(ContentBlock::text("Hel"));
    frames.push(frame(&mut encoder, E::TextStart { content_index: 0, partial: partial.clone() }));
    let covered = E::TextDelta { content_index: 0, delta: "He".into(), partial: partial.clone() };
    assert_eq!(encoder.encode(&covered).unwrap(), None);
    let remainder = frame(&mut encoder, E::TextDelta { content_index: 0, delta: "llo".into(), partial });
    assert_eq!(remainder, AssistantMessageFrame::TextDelta { content_index: 0, delta: "lo".into() });
    frames.push(remainder);
    assert_eq!(content_json(&frames), json!([{ "type": "text", "text": "Hello" }]));
}

#[test]
fn checkpoints_queued_tool_json_without_replaying_covered_deltas() {
    let mut partial = seed();
    partial.content.push(tool_call("call", "write", json!({ "path": "README.md" })));
    let events = vec![
        E::Start { partial: seed() },
        E::ToolcallStart { content_index: 0, partial: partial.clone() },
        E::ToolcallDelta { content_index: 0, delta: "{\"path\":\"READ".into(), partial: partial.clone() },
        E::ToolcallDelta { content_index: 0, delta: "ME.md\"}".into(), partial },
    ];
    let frames = encode_all(&events);
    assert_eq!(types(&frames), ["start", "toolcall_start", "toolcall_checkpoint"]);
    let checkpoint =
        AssistantMessageFrame::ToolcallCheckpoint { content_index: 0, json: "{\"path\":\"README.md\"}".into() };
    assert_eq!(frames[2], checkpoint);
    assert_eq!(
        content_json(&frames),
        json!([{ "type": "toolCall", "id": "call", "name": "write", "arguments": { "path": "README.md" } }])
    );
}

#[test]
fn resumes_legacy_grammar_tool_json_from_initial_arguments() {
    let mut partial = seed();
    let mut encoder = AssistantMessageFrameEncoder::new();
    let mut frames = vec![frame(&mut encoder, E::Start { partial: partial.clone() })];
    partial.content.push(tool_call("call", "bash", json!({ "input": "a" })));
    frames.push(frame(&mut encoder, E::ToolcallStart { content_index: 0, partial: partial.clone() }));
    partial.content[0] = tool_call("call", "bash", json!({ "input": "ab" }));
    let delta = E::ToolcallDelta { content_index: 0, delta: "{\"input\":\"ab".into(), partial: partial.clone() };
    frames.push(frame(&mut encoder, delta));
    partial.content[0] = tool_call("call", "bash", json!({ "input": "abc" }));
    frames.push(frame(&mut encoder, E::ToolcallDelta { content_index: 0, delta: "c\"}".into(), partial }));
    assert_eq!(
        frames[2..],
        [
            AssistantMessageFrame::ToolcallCheckpoint { content_index: 0, json: "{\"input\":\"ab".into() },
            AssistantMessageFrame::ToolcallDelta { content_index: 0, delta: "c\"}".into() },
        ]
    );
    assert_eq!(content_json(&frames)[0]["arguments"], json!({ "input": "abc" }));
}

#[test]
fn streams_tool_json_compactly_from_an_empty_argument_start() {
    let mut partial = seed();
    let mut encoder = AssistantMessageFrameEncoder::new();
    let mut frames = vec![frame(&mut encoder, E::Start { partial: partial.clone() })];
    partial.content.push(tool_call("call", "bash", json!({})));
    frames.push(frame(&mut encoder, E::ToolcallStart { content_index: 0, partial: partial.clone() }));
    partial.content[0] = tool_call("call", "bash", json!({ "command": "ls -la /tmp" }));
    let delta = "{\"command\":\"ls -la /tmp\"}";
    frames.push(frame(&mut encoder, E::ToolcallDelta { content_index: 0, delta: delta.into(), partial }));
    assert_eq!(frames[2], AssistantMessageFrame::ToolcallDelta { content_index: 0, delta: delta.into() });
    assert_eq!(content_json(&frames)[0]["arguments"], json!({ "command": "ls -la /tmp" }));
}

#[test]
fn accepts_a_pre_generation_error_but_rejects_success_or_updates_before_start() {
    let mut failed = seed();
    failed.stop_reason = StopReason::Error;
    failed.error_message = Some("setup failed".into());
    let error = E::Error { reason: StopReason::Error, error: failed };
    assert_eq!(AssistantMessageFrameEncoder::new().encode(&error).unwrap(), None);

    let done = E::Done { reason: StopReason::Stop, message: seed() };
    let err = AssistantMessageFrameEncoder::new().encode(&done).unwrap_err();
    assert!(err.0.contains("done event appears before start"));
    let delta = E::TextDelta { content_index: 0, delta: "x".into(), partial: seed() };
    let err = AssistantMessageFrameEncoder::new().encode(&delta).unwrap_err();
    assert!(err.0.contains("text_delta event appears before start"));
}

#[test]
fn treats_end_signature_metadata_including_absence_as_authoritative() {
    let frames: Vec<AssistantMessageFrame> = serde_json::from_value(json!([
        { "type": "start", "partial": seed() },
        { "type": "text_start", "contentIndex": 0, "content": { "type": "text", "text": "", "textSignature": "stale-text" } },
        { "type": "text_end", "contentIndex": 0, "content": "" },
        { "type": "thinking_start", "contentIndex": 1, "content": { "type": "thinking", "thinking": "",
          "thinkingSignature": "stale-thinking", "redacted": true } },
        { "type": "thinking_end", "contentIndex": 1, "content": "", "thinkingSignature": "", "redacted": false },
        { "type": "toolcall_start", "contentIndex": 2, "toolCall": { "type": "toolCall", "id": "call", "name": "read",
          "arguments": {}, "thoughtSignature": "stale-tool", "namespace": "stale-namespace" } },
        { "type": "toolcall_end", "contentIndex": 2, "id": "call", "name": "read", "arguments": {} },
    ]))
    .unwrap();
    assert_eq!(
        content_json(&frames),
        json!([
            { "type": "text", "text": "" },
            { "type": "thinking", "thinking": "", "thinkingSignature": "" },
            { "type": "toolCall", "id": "call", "name": "read", "arguments": {} },
        ])
    );
}

#[test]
fn stores_authoritative_final_arguments_in_toolcall_end_frames() {
    let call = ToolCall {
        id: "call-1".into(),
        name: "read".into(),
        arguments: json!({ "path": "README.md" }).as_object().unwrap().clone(),
        thought_signature: Some("thought".into()),
        namespace: Some("files".into()),
    };
    let mut partial = seed();
    partial.content.push(ContentBlock::ToolCall(call.clone()));
    let mut encoder = AssistantMessageFrameEncoder::new();
    frame(&mut encoder, E::Start { partial: partial.clone() });
    frame(&mut encoder, E::ToolcallStart { content_index: 0, partial: partial.clone() });
    let end = frame(&mut encoder, E::ToolcallEnd { content_index: 0, tool_call: call, partial });
    assert_eq!(
        serde_json::to_value(end).unwrap(),
        json!({ "type": "toolcall_end", "contentIndex": 0, "id": "call-1", "name": "read",
            "arguments": { "path": "README.md" }, "thoughtSignature": "thought", "namespace": "files" })
    );
}

#[test]
fn supports_interleaved_streams_by_content_index() {
    let frames: Vec<AssistantMessageFrame> = serde_json::from_value(json!([
        { "type": "start", "partial": seed() },
        { "type": "text_start", "contentIndex": 0, "content": { "type": "text", "text": "" } },
        { "type": "toolcall_start", "contentIndex": 1, "toolCall": { "type": "toolCall", "id": "call", "name": "lookup", "arguments": {} } },
        { "type": "thinking_start", "contentIndex": 2, "content": { "type": "thinking", "thinking": "" } },
        { "type": "text_delta", "contentIndex": 0, "delta": "answer" },
        { "type": "toolcall_delta", "contentIndex": 1, "delta": "{\"query\":\"pi\"}" },
        { "type": "thinking_delta", "contentIndex": 2, "delta": "check" },
        { "type": "toolcall_end", "contentIndex": 1, "id": "call", "name": "lookup", "arguments": { "query": "pi" } },
        { "type": "text_end", "contentIndex": 0, "content": "answer" },
        { "type": "thinking_end", "contentIndex": 2, "content": "check" },
    ]))
    .unwrap();
    assert_eq!(
        content_json(&frames),
        json!([
            { "type": "text", "text": "answer" },
            { "type": "toolCall", "id": "call", "name": "lookup", "arguments": { "query": "pi" } },
            { "type": "thinking", "thinking": "check" },
        ])
    );
}

#[test]
fn omits_terminal_events_and_rejects_events_after_them() {
    let mut encoder = AssistantMessageFrameEncoder::new();
    encoder.encode(&E::Start { partial: seed() }).unwrap();
    assert_eq!(encoder.encode(&E::Done { reason: StopReason::Stop, message: seed() }).unwrap(), None);
    let late = encoder.encode(&E::Start { partial: seed() }).unwrap_err();
    assert_eq!(late.0, "Assistant message event start follows a terminal event");
}

#[test]
fn returns_none_without_a_start_frame() {
    assert_eq!(reduce(&[]), None);
    assert_eq!(reduce(&[AssistantMessageFrame::TextDelta { content_index: 0, delta: "x".into() }]), None);
}

#[test]
fn rejects_frames_before_start_wrong_block_kinds_duplicate_ends_and_index_gaps() {
    let before =
        json!([{ "type": "text_delta", "contentIndex": 0, "delta": "x" }, { "type": "start", "partial": seed() }]);
    assert!(reduce_err(before).contains("before the start frame"));
    let wrong = json!([
        { "type": "start", "partial": seed() },
        { "type": "toolcall_start", "contentIndex": 0, "toolCall": { "type": "toolCall", "id": "call", "name": "run", "arguments": {} } },
        { "type": "text_delta", "contentIndex": 0, "delta": "wrong" },
    ]);
    assert!(reduce_err(wrong).contains("expected text block"));
    let twice = json!([
        { "type": "start", "partial": seed() },
        { "type": "text_start", "contentIndex": 0, "content": { "type": "text", "text": "" } },
        { "type": "text_end", "contentIndex": 0, "content": "" },
        { "type": "text_end", "contentIndex": 0, "content": "" },
    ]);
    assert!(reduce_err(twice).contains("follows the end"));
    let gap = json!([
        { "type": "start", "partial": seed() },
        { "type": "text_start", "contentIndex": 1, "content": { "type": "text", "text": "" } },
    ]);
    assert!(reduce_err(gap).contains("would leave a gap"));
}

#[test]
fn rejects_events_whose_content_index_points_to_the_wrong_block_kind() {
    let mut partial = seed();
    let mut encoder = AssistantMessageFrameEncoder::new();
    encoder.encode(&E::Start { partial: partial.clone() }).unwrap();
    partial.content.push(ContentBlock::Thinking { thinking: String::new(), thinking_signature: None, redacted: false });
    let err = encoder.encode(&E::TextStart { content_index: 0, partial }).unwrap_err();
    assert!(err.0.contains("text_start event points to thinking block"), "{err}");
}
