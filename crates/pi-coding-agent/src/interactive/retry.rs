//! `/retry` (pi-rust extension): run the newest prompt of the current branch again. The leaf
//! moves to the prompt's parent as `/tree` moves it, so the old answer stays in the tree.

use std::rc::Rc;
use std::sync::Arc;

use pi_ai::{ContentBlock, UserContent};

use super::mode::InteractiveMode;
use crate::agent_session::{AgentSession, BranchPrompt, NavigateTreeOptions};

const BUSY: &str = "Wait for the current response to finish, or press Esc to abort it, before /retry.";
const COMPACTING: &str = "Wait for compaction to finish before /retry.";

/// The text and images of a prompt, in the form a new prompt takes them.
fn prompt_parts(content: &UserContent) -> (String, Vec<ContentBlock>) {
    let images = match content {
        UserContent::Text(_) => Vec::new(),
        UserContent::Blocks(blocks) => {
            blocks.iter().filter(|block| matches!(block, ContentBlock::Image { .. })).cloned().collect()
        }
    };
    (content.text(), images)
}

/// Move the leaf to the parent of `prompt`. False when an extension cancelled the move.
async fn branch_before(session: Arc<AgentSession>, prompt: BranchPrompt) -> anyhow::Result<bool> {
    let at_prompt = session.session_manager().leaf_id() == Some(prompt.entry_id.as_str());
    if !at_prompt {
        let result = session.navigate_tree(&prompt.entry_id, NavigateTreeOptions::default()).await?;
        return Ok(!result.cancelled);
    }
    // `navigate_tree` does not move when the leaf is its target, as for a prompt with no answer.
    if session.is_streaming() || session.is_compacting() {
        anyhow::bail!(BUSY);
    }
    {
        let mut manager = session.session_manager();
        match &prompt.parent_id {
            Some(parent) => manager.branch(parent)?,
            None => manager.reset_leaf(),
        }
    }
    session.refresh_context();
    Ok(true)
}

impl InteractiveMode {
    /// `/retry`: refused while the agent works and before the first prompt.
    pub(super) fn handle_retry_command(&self) {
        let session = self.session();
        if session.is_streaming() {
            return self.show_error(BUSY);
        }
        if session.is_compacting() {
            return self.show_error(COMPACTING);
        }
        let Some(prompt) = session.last_user_prompt() else {
            return self.show_error("Nothing to retry: the current branch has no prompt yet.");
        };
        self.spawn(move |mode| async move { mode.retry_prompt(prompt).await });
    }

    async fn retry_prompt(self: Rc<Self>, prompt: BranchPrompt) {
        let (text, images) = prompt_parts(&prompt.message.content);
        let moved = tokio::spawn(branch_before(self.session(), prompt)).await;
        match moved.map_err(anyhow::Error::from).and_then(|result| result) {
            Ok(true) => {
                self.ui.chat.borrow_mut().clear();
                self.render_initial_messages();
                self.queue_prompt(text, images);
            }
            Ok(false) => self.show_status("Retry cancelled"),
            Err(error) => self.show_error(&format!("{error:#}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_text_and_images_of_a_prompt() {
        let image = ContentBlock::Image { data: "aGk=".into(), mime_type: "image/png".into() };
        let blocks = UserContent::Blocks(vec![ContentBlock::text("look at this"), image.clone()]);
        assert_eq!(prompt_parts(&blocks), ("look at this".to_string(), vec![image]));
        assert_eq!(prompt_parts(&UserContent::Text("plain".into())), ("plain".to_string(), Vec::new()));
    }
}
