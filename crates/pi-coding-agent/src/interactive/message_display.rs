//! The pi-rust `message_display` event: extensions change how the text of an assistant message
//! shows in the TUI. The session, exports, and the model context keep the original text.

use std::collections::HashMap;
use std::time::Duration;

use pi_ai::{AssistantMessage, ContentBlock, Message};

use super::components::AssistantMessageComponent;
use super::mode::InteractiveMode;
use crate::agent_session::AgentSession;
use crate::extensions::MESSAGE_DISPLAY_LIMIT;

/// Display texts by message timestamp. Chat rebuilds and redraws use them and do not ask again.
#[derive(Default)]
pub(super) struct MessageDisplayCache {
    entries: HashMap<i64, Cached>,
}

struct Cached {
    original: String,
    /// `None` while the handlers run; `Some(None)` keeps the original.
    replacement: Option<Option<String>>,
}

/// The text the event sends: the non-empty text blocks, trimmed and joined by a blank line.
pub(super) fn display_source(message: &AssistantMessage) -> Option<String> {
    let blocks: Vec<&str> = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } if !text.trim().is_empty() => Some(text.trim()),
            _ => None,
        })
        .collect();
    (!blocks.is_empty()).then(|| blocks.join("\n\n"))
}

/// How long the event waits for the session to save the message, in steps of `ENTRY_WAIT_STEP`.
const ENTRY_WAIT_STEPS: u32 = 20;
const ENTRY_WAIT_STEP: Duration = Duration::from_millis(25);

/// The session emits `message_end` before it saves the message, so the entry can come a moment
/// after the event; a message that is never saved gets `None` after the wait.
async fn wait_for_entry_id(session: &AgentSession, timestamp: i64) -> Option<String> {
    for _ in 0..ENTRY_WAIT_STEPS {
        if let Some(id) = saved_entry_id(session, timestamp) {
            return Some(id);
        }
        tokio::time::sleep(ENTRY_WAIT_STEP).await;
    }
    saved_entry_id(session, timestamp)
}

/// The session entry id of the saved assistant message with `timestamp`.
fn saved_entry_id(session: &AgentSession, timestamp: i64) -> Option<String> {
    let manager = session.session_manager();
    let branch = manager.get_branch(None);
    let entry =
        branch.iter().rev().take(64).find(
            |entry| matches!(entry.message(), Some(Message::Assistant(message)) if message.timestamp == timestamp),
        )?;
    Some(entry.id().to_string())
}

impl InteractiveMode {
    /// Show the known display text of the complete message of `component`, or ask the
    /// `message_display` handlers for one. The original shows until they answer.
    pub(super) fn apply_message_display(&self, component: &mut AssistantMessageComponent, entry_id: Option<String>) {
        let Some(message) = component.message() else { return };
        let key = message.timestamp;
        let Some(text) = display_source(message) else { return };
        let known = {
            let state = self.state.borrow();
            state
                .message_display
                .entries
                .get(&key)
                .filter(|cached| cached.original == text)
                .map(|cached| cached.replacement.clone())
        };
        if let Some(replacement) = known {
            if let Some(Some(replacement)) = replacement {
                component.set_display_text(Some(replacement));
            }
            return;
        }
        let session = self.session();
        let Some(runner) = session.extension_runner().filter(|runner| runner.listens_to_message_display()) else {
            return;
        };
        let pending = Cached { original: text.clone(), replacement: None };
        self.state.borrow_mut().message_display.entries.insert(key, pending);
        self.spawn(move |mode| async move {
            let entry_id = match entry_id {
                Some(id) => Some(id),
                None => wait_for_entry_id(&session, key).await,
            };
            let replacement = runner.message_display(entry_id.as_deref(), &text, MESSAGE_DISPLAY_LIMIT).await;
            mode.finish_message_display(key, text, replacement);
        });
    }

    /// Store the handlers' answer and redraw the messages it belongs to.
    fn finish_message_display(&self, key: i64, original: String, replacement: Option<String>) {
        let cached = Cached { original: original.clone(), replacement: Some(replacement.clone()) };
        self.state.borrow_mut().message_display.entries.insert(key, cached);
        let Some(replacement) = replacement else { return };
        let children = self.ui.chat.borrow().children.clone();
        for child in &children {
            let Ok(mut child) = child.try_borrow_mut() else { continue };
            let Some(message) = child.as_any_mut().downcast_mut::<AssistantMessageComponent>() else { continue };
            let matches = message
                .message()
                .is_some_and(|shown| shown.timestamp == key && display_source(shown).as_ref() == Some(&original));
            if matches {
                message.set_display_text(Some(replacement.clone()));
            }
        }
        self.request_render();
    }
}
