//! Markdown transformer hooks (upstream `components/markdown-transform.ts` and the extension
//! `MarkdownTransformer` type). Transformers rewrite message Markdown before it is rendered.

mod mermaid;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

pub use mermaid::{ActiveThemeStyle, MermaidStyle, MermaidTransformerOptions, create_mermaid_markdown_transformer};

/// Which message the Markdown belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MarkdownMessageType {
    #[serde(rename = "user")]
    User,
    #[serde(rename = "assistant")]
    Assistant,
    #[serde(rename = "assistant-thinking")]
    AssistantThinking,
}

/// Where and how the Markdown is shown (upstream `MarkdownTransformContext`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkdownTransformContext {
    pub message_type: MarkdownMessageType,
    pub is_streaming: bool,
    /// Columns available for the rendered content.
    pub available_width: usize,
    /// Persisted session-entry id. `None` for live messages and live rendering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// Persisted session-entry ISO timestamp. `None` for live messages and live rendering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
}

/// The persisted session entry a message is rendered from (upstream `messageMeta`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageMeta {
    pub id: String,
    pub timestamp: String,
}

/// Rewrites Markdown. `None` keeps the input, like a transformer that returns a non-string upstream.
pub type MarkdownTransformer = Arc<dyn Fn(&str, &MarkdownTransformContext) -> Option<String> + Send + Sync>;

/// The Markdown component's transform hook: `(markdown, available width) -> markdown`. Same shape as
/// pi-tui's `MarkdownTransformFn`; replace with that alias once the Markdown component lands.
pub type MarkdownTransformFn = Rc<dyn Fn(&str, usize) -> String>;

/// Run transformers in order. A transformer that panics or returns `None` leaves the Markdown as it was.
pub fn apply_markdown_transformers(
    markdown: &str,
    context: &MarkdownTransformContext,
    transformers: &[MarkdownTransformer],
) -> String {
    let mut transformed = markdown.to_string();
    for transformer in transformers {
        let result = catch_unwind(AssertUnwindSafe(|| transformer(&transformed, context)));
        if let Ok(Some(next)) = result {
            transformed = next;
        }
    }
    transformed
}

/// The transform hook for one message (upstream `createMarkdownTransform`). `meta` names the
/// session entry of a replayed message.
pub fn create_markdown_transform(
    message_type: MarkdownMessageType,
    is_streaming: bool,
    transformers: Vec<MarkdownTransformer>,
    meta: Option<MessageMeta>,
) -> MarkdownTransformFn {
    Rc::new(move |markdown: &str, available_width: usize| {
        let context = MarkdownTransformContext {
            message_type,
            is_streaming,
            available_width,
            message_id: meta.as_ref().map(|meta| meta.id.clone()),
            timestamp: meta.as_ref().map(|meta| meta.timestamp.clone()),
        };
        apply_markdown_transformers(markdown, &context, &transformers)
    })
}

#[cfg(test)]
mod tests;
