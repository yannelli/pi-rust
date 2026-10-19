//! History normalization before conversion, ported from upstream `api/transform-messages.ts`:
//! downgrade unsupported images, drop foreign thinking, normalize foreign tool call
//! ids, drop failed turns, and close tool calls that never got a result.

use std::collections::HashMap;

use crate::types::{
    AssistantMessage, ContentBlock, Message, Model, StopReason, ToolResultMessage, UserContent, now_ms,
};

const USER_IMAGE_PLACEHOLDER: &str = "(image omitted: model does not support images)";
const TOOL_IMAGE_PLACEHOLDER: &str = "(tool image omitted: model does not support images)";
const ERROR_TOOL_IMAGE_PLACEHOLDER: &str = "(image attached after the tool results)";

/// An error tool result split into text-only content and the images to send after all tool results.
pub(crate) struct SplitErrorToolResult {
    pub content: Vec<ContentBlock>,
    /// Names the tool call; goes before `images`.
    pub caption: String,
    pub images: Vec<ContentBlock>,
}

/// Split the images out of an error tool result, for APIs that reject images in error results
/// (Anthropic, and Claude on Bedrock). `None` when the result needs no split.
pub(crate) fn split_error_tool_result_images(result: &ToolResultMessage) -> Option<SplitErrorToolResult> {
    if !result.is_error {
        return None;
    }
    let images: Vec<ContentBlock> =
        result.content.iter().filter(|block| matches!(block, ContentBlock::Image { .. })).cloned().collect();
    if images.is_empty() {
        return None;
    }
    Some(SplitErrorToolResult {
        content: replace_images(&result.content, ERROR_TOOL_IMAGE_PLACEHOLDER),
        caption: format!("Images from the error result of tool call {} ({}):", result.tool_call_id, result.tool_name),
        images,
    })
}

fn replace_images(content: &[ContentBlock], placeholder: &str) -> Vec<ContentBlock> {
    let mut result = Vec::new();
    let mut previous_was_placeholder = false;
    for block in content {
        if matches!(block, ContentBlock::Image { .. }) {
            if !previous_was_placeholder {
                result.push(ContentBlock::text(placeholder));
            }
            previous_was_placeholder = true;
            continue;
        }
        previous_was_placeholder = block.as_text() == Some(placeholder);
        result.push(block.clone());
    }
    result
}

/// Prepare history for `model`. System messages pass through; one that arrives while tool
/// calls wait for results moves after those results. App-defined messages are dropped.
pub(crate) fn transform_messages(
    messages: &[Message],
    model: &Model,
    normalize_id: &dyn Fn(&str, &AssistantMessage) -> String,
) -> Vec<Message> {
    let images = model.supports_images();
    let mut id_map: HashMap<String, String> = HashMap::new();
    let mut transformed = Vec::with_capacity(messages.len());
    for message in messages {
        match message {
            Message::Custom(_) => {}
            Message::System(_) => transformed.push(message.clone()),
            Message::User(user) => {
                let mut user = user.clone();
                if let (false, UserContent::Blocks(blocks)) = (images, &user.content) {
                    user.content = UserContent::Blocks(replace_images(blocks, USER_IMAGE_PLACEHOLDER));
                }
                transformed.push(Message::User(user));
            }
            Message::ToolResult(result) => {
                let mut result = result.clone();
                if !images {
                    result.content = replace_images(&result.content, TOOL_IMAGE_PLACEHOLDER);
                }
                if let Some(id) = id_map.get(&result.tool_call_id) {
                    result.tool_call_id = id.clone();
                }
                transformed.push(Message::ToolResult(result));
            }
            Message::Assistant(assistant) => {
                let same_model =
                    assistant.provider == model.provider && assistant.api == model.api && assistant.model == model.id;
                let mut converted = assistant.clone();
                converted.content = assistant
                    .content
                    .iter()
                    .filter_map(|block| transform_block(block, assistant, same_model, normalize_id, &mut id_map))
                    .collect();
                transformed.push(Message::Assistant(converted));
            }
        }
    }
    close_orphaned_tool_calls(transformed)
}

fn transform_block(
    block: &ContentBlock,
    source: &AssistantMessage,
    same_model: bool,
    normalize_id: &dyn Fn(&str, &AssistantMessage) -> String,
    id_map: &mut HashMap<String, String>,
) -> Option<ContentBlock> {
    match block {
        ContentBlock::Thinking { thinking, thinking_signature, redacted } => {
            // Another model's thinking would read as part of its visible reply, and its signatures are invalid here.
            if !same_model {
                return None;
            }
            let signed = thinking_signature.as_deref().is_some_and(|signature| !signature.is_empty());
            if *redacted || signed {
                return Some(block.clone());
            }
            (!thinking.trim().is_empty()).then(|| block.clone())
        }
        ContentBlock::Text { text, .. } if !same_model => Some(ContentBlock::text(text.clone())),
        ContentBlock::ToolCall(call) if !same_model => {
            let mut call = call.clone();
            call.thought_signature = None;
            let id = normalize_id(&call.id, source);
            if id != call.id {
                id_map.insert(call.id.clone(), id.clone());
                call.id = id;
            }
            Some(ContentBlock::ToolCall(call))
        }
        _ => Some(block.clone()),
    }
}

fn missing_result(id: String, name: String) -> Message {
    Message::ToolResult(ToolResultMessage {
        tool_call_id: id,
        tool_name: name,
        content: vec![ContentBlock::text("No result provided")],
        details: None,
        usage: None,
        nested_calls: None,
        is_error: true,
        timestamp: now_ms(),
        duration_ms: None,
    })
}

fn close_orphaned_tool_calls(messages: Vec<Message>) -> Vec<Message> {
    let mut result = Vec::with_capacity(messages.len());
    let mut pending: Vec<(String, String)> = Vec::new();
    let mut held: Vec<Message> = Vec::new();
    let flush = |pending: &mut Vec<(String, String)>, held: &mut Vec<Message>, result: &mut Vec<Message>| {
        result.extend(pending.drain(..).map(|(id, name)| missing_result(id, name)));
        result.append(held);
    };
    for message in messages {
        match &message {
            Message::Assistant(assistant) => {
                flush(&mut pending, &mut held, &mut result);
                if matches!(assistant.stop_reason, StopReason::Error | StopReason::Aborted) {
                    continue;
                }
                pending = assistant.tool_calls().map(|call| (call.id.clone(), call.name.clone())).collect();
            }
            Message::ToolResult(tool_result) => pending.retain(|(id, _)| *id != tool_result.tool_call_id),
            Message::System(_) if !pending.is_empty() => {
                held.push(message);
                continue;
            }
            Message::System(_) => {}
            Message::User(_) | Message::Custom(_) => flush(&mut pending, &mut held, &mut result),
        }
        result.push(message);
    }
    flush(&mut pending, &mut held, &mut result);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{SystemMessage, ToolCall};

    fn assistant(stop_reason: StopReason, id: &str) -> Message {
        Message::Assistant(AssistantMessage {
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: id.into(),
                name: "read".into(),
                ..Default::default()
            })],
            api: "other".into(),
            provider: "other".into(),
            model: "x".into(),
            stop_reason,
            ..Default::default()
        })
    }

    fn system(text: &str) -> Message {
        Message::System(SystemMessage {
            content: UserContent::Text(text.into()),
            sections: None,
            tools_added: None,
            tools_removed: None,
            timestamp: 1,
        })
    }

    #[test]
    fn adds_missing_tool_results_and_drops_failed_turns() {
        let model = crate::providers::faux::FauxProvider::model();
        let messages =
            vec![assistant(StopReason::Aborted, "a|1"), assistant(StopReason::ToolUse, "b|2"), Message::user("next")];
        let normalize = |id: &str, _: &AssistantMessage| id.replace('|', "_");
        let result = transform_messages(&messages, &model, &normalize);
        assert_eq!(result.len(), 3);
        let Message::ToolResult(synthetic) = &result[1] else { panic!("expected tool result") };
        assert_eq!(synthetic.tool_call_id, "b_2");
        assert!(synthetic.is_error);
    }

    #[test]
    fn system_messages_wait_for_pending_tool_results() {
        let model = crate::providers::faux::FauxProvider::model();
        let messages = vec![
            system("lead"),
            assistant(StopReason::ToolUse, "c1"),
            system("update"),
            Message::ToolResult(ToolResultMessage { tool_call_id: "c1".into(), ..Default::default() }),
            Message::user("next"),
        ];
        let roles: Vec<String> = transform_messages(&messages, &model, &|id, _| id.to_string())
            .iter()
            .map(|message| message.role().to_string())
            .collect();
        assert_eq!(roles, ["system", "assistant", "toolResult", "system", "user"]);
    }
}
