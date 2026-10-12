//! GitHub Copilot request headers, ported from upstream `api/github-copilot-headers.ts`
//! and the Copilot branch of `anthropic-messages.ts`. The OAuth stream supplies the token
//! as `api_key` and the account's base URL on the model.

use crate::types::{ContentBlock, Message, Model, UserContent};

pub const GITHUB_COPILOT_PROVIDER: &str = "github-copilot";

/// `user` when the last message is from the user, else `agent` (follow-ups after tools).
pub fn infer_copilot_initiator(messages: &[Message]) -> &'static str {
    match messages.last() {
        Some(last) if last.role() != "user" => "agent",
        _ => "user",
    }
}

/// Copilot requires `Copilot-Vision-Request` when user or tool result content has images.
pub fn has_copilot_vision_input(messages: &[Message]) -> bool {
    let has_image = |blocks: &[ContentBlock]| blocks.iter().any(|block| matches!(block, ContentBlock::Image { .. }));
    messages.iter().any(|message| match message {
        Message::User(user) => matches!(&user.content, UserContent::Blocks(blocks) if has_image(blocks)),
        Message::ToolResult(result) => has_image(&result.content),
        _ => false,
    })
}

/// `X-Initiator`, `Openai-Intent`, and `Copilot-Vision-Request` for one request.
pub fn build_copilot_dynamic_headers(messages: &[Message], has_images: bool) -> Vec<(&'static str, String)> {
    let mut headers = vec![
        ("X-Initiator", infer_copilot_initiator(messages).to_string()),
        ("Openai-Intent", "conversation-edits".to_string()),
    ];
    if has_images {
        headers.push(("Copilot-Vision-Request", "true".to_string()));
    }
    headers
}

/// The dynamic headers for `model`, empty unless it is a GitHub Copilot model.
/// The anthropic, openai-completions, and openai-responses adapters add these.
pub fn copilot_dynamic_headers(model: &Model, messages: &[Message]) -> Vec<(&'static str, String)> {
    if model.provider != GITHUB_COPILOT_PROVIDER {
        return Vec::new();
    }
    build_copilot_dynamic_headers(messages, has_copilot_vision_input(messages))
}

/// Anthropic Messages auth for Copilot: a Bearer token instead of `x-api-key`, plus the
/// static headers upstream sets on its Copilot client.
pub fn copilot_anthropic_auth_headers(api_key: Option<&str>) -> Vec<(&'static str, String)> {
    let mut headers = vec![
        ("accept", "application/json".to_string()),
        ("anthropic-dangerous-direct-browser-access", "true".to_string()),
    ];
    if let Some(key) = api_key.filter(|key| !key.is_empty()) {
        headers.push(("authorization", format!("Bearer {key}")));
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AssistantMessage, ToolResultMessage, UserMessage};

    fn copilot_model() -> Model {
        Model { id: "claude-sonnet-4.6".into(), provider: GITHUB_COPILOT_PROVIDER.into(), ..Model::default() }
    }

    fn image_user() -> Message {
        Message::User(UserMessage {
            content: UserContent::Blocks(vec![ContentBlock::Image {
                data: "AA==".into(),
                mime_type: "image/png".into(),
            }]),
            timestamp: 0,
        })
    }

    #[test]
    fn builds_initiator_intent_and_vision_headers() {
        let model = copilot_model();
        let headers = copilot_dynamic_headers(&model, &[Message::user("hi")]);
        assert_eq!(
            headers,
            vec![("X-Initiator", "user".to_string()), ("Openai-Intent", "conversation-edits".to_string())]
        );

        let tool_result = Message::ToolResult(ToolResultMessage {
            content: vec![ContentBlock::Image { data: "AA==".into(), mime_type: "image/png".into() }],
            ..ToolResultMessage::default()
        });
        let messages = [Message::user("hi"), Message::Assistant(AssistantMessage::default()), tool_result];
        let headers = copilot_dynamic_headers(&model, &messages);
        assert_eq!(headers[0], ("X-Initiator", "agent".to_string()));
        assert_eq!(headers[2], ("Copilot-Vision-Request", "true".to_string()));
        assert!(has_copilot_vision_input(&[image_user()]));
        assert!(!has_copilot_vision_input(&[Message::user("text only")]));
        assert_eq!(infer_copilot_initiator(&[]), "user");

        let other = Model { provider: "anthropic".into(), ..copilot_model() };
        assert!(copilot_dynamic_headers(&other, &messages).is_empty());
    }

    #[test]
    fn anthropic_copilot_auth_uses_bearer_tokens() {
        let headers = copilot_anthropic_auth_headers(Some("tid_copilot"));
        assert!(headers.contains(&("authorization", "Bearer tid_copilot".to_string())));
        assert!(headers.contains(&("anthropic-dangerous-direct-browser-access", "true".to_string())));
        assert_eq!(copilot_anthropic_auth_headers(None).len(), 2);
    }
}
