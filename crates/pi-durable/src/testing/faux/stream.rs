//! Token streaming of upstream `faux.ts` `streamWithDeltas`: the same events in the same order, with chunks of
//! `token_size` tokens (4 characters each) and optional pacing in tokens per second.

use std::time::Duration;

use pi_ai::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, EventSender, JsonObject, StopReason, ToolCall, now_ms,
};
use pi_chord::AbortSignal;

use super::usage::{estimate_tokens, json_stringify};
use crate::util::random_f64;

/// Chunk sizes and pacing of one faux provider.
#[derive(Clone, Copy, Debug)]
pub(super) struct Pacing {
    pub min_token_size: u32,
    pub max_token_size: u32,
    pub tokens_per_second: Option<f64>,
}

/// Split `text` into chunks of a random token size between the bounds, 4 UTF-16 units per token. A surrogate pair
/// is never split, so a chunk can be one unit longer than its size.
fn split_by_token_size(text: &str, pacing: Pacing) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut chars = text.chars().peekable();
    while chars.peek().is_some() {
        let span = f64::from(pacing.max_token_size - pacing.min_token_size + 1);
        let token_size = pacing.min_token_size + (random_f64() * span).floor() as u32;
        let char_size = (token_size as usize * 4).max(1);
        let mut chunk = String::new();
        let mut units = 0;
        while units < char_size {
            let Some(next) = chars.next() else { break };
            units += next.len_utf16();
            chunk.push(next);
        }
        chunks.push(chunk);
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    chunks
}

/// Upstream `scheduleChunk`: a microtask without pacing, else the chunk's share of `tokens_per_second`.
async fn schedule_chunk(chunk: &str, tokens_per_second: Option<f64>) {
    match tokens_per_second.filter(|rate| *rate > 0.0) {
        None => tokio::task::yield_now().await,
        Some(rate) => tokio::time::sleep(Duration::from_secs_f64(estimate_tokens(chunk) as f64 / rate)).await,
    }
}

fn aborted_message(partial: &AssistantMessage) -> AssistantMessage {
    AssistantMessage {
        stop_reason: StopReason::Aborted,
        error_message: Some("Request was aborted".into()),
        timestamp: now_ms(),
        ..partial.clone()
    }
}

fn is_aborted(signal: Option<&AbortSignal>) -> bool {
    signal.is_some_and(AbortSignal::aborted)
}

/// Push the terminal `Error` event of an aborted stream.
fn push_aborted(sender: &EventSender, partial: &AssistantMessage) {
    sender.push(AssistantMessageEvent::Error { reason: StopReason::Aborted, error: aborted_message(partial) });
}

/// Stream `message` block by block. `Err` means the message has no terminal stop reason; the caller reports it.
pub(super) async fn stream_with_deltas(
    sender: &EventSender,
    message: &AssistantMessage,
    pacing: Pacing,
    signal: Option<&AbortSignal>,
) -> Result<(), String> {
    let mut partial = AssistantMessage { content: Vec::new(), stop_reason: StopReason::Pending, ..message.clone() };
    if is_aborted(signal) {
        push_aborted(sender, &partial);
        return Ok(());
    }
    sender.push(AssistantMessageEvent::Start { partial: partial.clone() });

    for (index, block) in message.content.iter().enumerate() {
        if is_aborted(signal) {
            push_aborted(sender, &partial);
            return Ok(());
        }
        match block {
            ContentBlock::Thinking { thinking, .. } => {
                partial.content.push(ContentBlock::Thinking {
                    thinking: String::new(),
                    thinking_signature: None,
                    redacted: false,
                });
                sender.push(AssistantMessageEvent::ThinkingStart { content_index: index, partial: partial.clone() });
                for chunk in split_by_token_size(thinking, pacing) {
                    schedule_chunk(&chunk, pacing.tokens_per_second).await;
                    if is_aborted(signal) {
                        push_aborted(sender, &partial);
                        return Ok(());
                    }
                    if let ContentBlock::Thinking { thinking, .. } = &mut partial.content[index] {
                        thinking.push_str(&chunk);
                    }
                    let partial = partial.clone();
                    sender.push(AssistantMessageEvent::ThinkingDelta { content_index: index, delta: chunk, partial });
                }
                sender.push(AssistantMessageEvent::ThinkingEnd {
                    content_index: index,
                    content: thinking.clone(),
                    partial: partial.clone(),
                });
            }
            ContentBlock::Text { text, .. } => {
                partial.content.push(ContentBlock::text(""));
                sender.push(AssistantMessageEvent::TextStart { content_index: index, partial: partial.clone() });
                for chunk in split_by_token_size(text, pacing) {
                    schedule_chunk(&chunk, pacing.tokens_per_second).await;
                    if is_aborted(signal) {
                        push_aborted(sender, &partial);
                        return Ok(());
                    }
                    if let ContentBlock::Text { text, .. } = &mut partial.content[index] {
                        text.push_str(&chunk);
                    }
                    let partial = partial.clone();
                    sender.push(AssistantMessageEvent::TextDelta { content_index: index, delta: chunk, partial });
                }
                sender.push(AssistantMessageEvent::TextEnd {
                    content_index: index,
                    content: text.clone(),
                    partial: partial.clone(),
                });
            }
            ContentBlock::ToolCall(call) => {
                partial.content.push(ContentBlock::ToolCall(ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: JsonObject::new(),
                    ..ToolCall::default()
                }));
                sender.push(AssistantMessageEvent::ToolcallStart { content_index: index, partial: partial.clone() });
                for chunk in split_by_token_size(&json_stringify(&call.arguments), pacing) {
                    schedule_chunk(&chunk, pacing.tokens_per_second).await;
                    if is_aborted(signal) {
                        push_aborted(sender, &partial);
                        return Ok(());
                    }
                    let partial = partial.clone();
                    sender.push(AssistantMessageEvent::ToolcallDelta { content_index: index, delta: chunk, partial });
                }
                if let ContentBlock::ToolCall(open) = &mut partial.content[index] {
                    open.arguments = call.arguments.clone();
                }
                sender.push(AssistantMessageEvent::ToolcallEnd {
                    content_index: index,
                    tool_call: call.clone(),
                    partial: partial.clone(),
                });
            }
            ContentBlock::Image { .. } => {
                // Upstream faux streams only text, thinking, and tool calls; an image block streams whole.
                partial.content.push(block.clone());
            }
        }
    }

    match message.stop_reason {
        StopReason::Pending => Err("Faux response ended without a stop reason".into()),
        reason @ (StopReason::Error | StopReason::Aborted) => {
            sender.push(AssistantMessageEvent::Error { reason, error: message.clone() });
            Ok(())
        }
        reason => {
            sender.push(AssistantMessageEvent::Done { reason, message: message.clone() });
            Ok(())
        }
    }
}
