//! The request context that the built-in Antigravity provider builds its session from, before a prompt.

use pi_ai::{Context, Message};

use super::AgentSession;

impl AgentSession {
    /// The provider context of the next turn as known before a prompt: the session messages and the prompt
    /// and tool patch of the run's options (MCP ready, no `before_agent_start` handlers).
    pub(crate) async fn antigravity_context(&self) -> Context {
        let options = self.turn_prompt_options().await;
        let mut messages = self.projected_context().messages;
        if let Some(update) = self.prepare_prompt_and_tool_loadout(&options, &messages) {
            messages.push(Message::System(update));
        }
        Context { system_prompt: None, messages: crate::messages::convert_to_llm(&messages), tools: Vec::new() }
    }
}
