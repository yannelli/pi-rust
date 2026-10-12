//! `images.blockImages`: replace image blocks sent to the model with a text placeholder
//! (upstream `convertToLlmWithBlockImages` in `core/sdk.ts`).

use pi_ai::{ContentBlock, Message, UserContent};

pub const BLOCKED_IMAGE_TEXT: &str = "Image reading is disabled.";

fn is_placeholder(block: &ContentBlock) -> bool {
    block.as_text() == Some(BLOCKED_IMAGE_TEXT)
}

fn replace_images(content: &[ContentBlock]) -> Option<Vec<ContentBlock>> {
    if !content.iter().any(|block| matches!(block, ContentBlock::Image { .. })) {
        return None;
    }
    let mapped: Vec<ContentBlock> = content
        .iter()
        .map(|block| match block {
            ContentBlock::Image { .. } => ContentBlock::text(BLOCKED_IMAGE_TEXT),
            other => other.clone(),
        })
        .collect();
    // Drop a placeholder that directly follows another one in the mapped list.
    let filtered = mapped
        .iter()
        .enumerate()
        .filter(|(i, block)| !(is_placeholder(block) && *i > 0 && is_placeholder(&mapped[i - 1])))
        .map(|(_, block)| block.clone())
        .collect();
    Some(filtered)
}

/// Apply the filter to messages already converted for the provider (user and tool results).
pub fn block_images(messages: Vec<Message>) -> Vec<Message> {
    messages
        .into_iter()
        .map(|message| match message {
            Message::User(mut user) => {
                let filtered = match &user.content {
                    UserContent::Blocks(blocks) => replace_images(blocks),
                    UserContent::Text(_) => None,
                };
                if let Some(filtered) = filtered {
                    user.content = UserContent::Blocks(filtered);
                }
                Message::User(user)
            }
            Message::ToolResult(mut result) => {
                if let Some(filtered) = replace_images(&result.content) {
                    result.content = filtered;
                }
                Message::ToolResult(result)
            }
            other => other,
        })
        .collect()
}
