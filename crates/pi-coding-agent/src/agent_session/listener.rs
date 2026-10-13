//! Agent event handling (upstream `_handleAgentEvent`): queue bookkeeping, forwarding to
//! session listeners, persistence of finished messages, and retry and turn tracking.

use pi_agent::AgentEvent;
use pi_ai::{ContentBlock, Message, StopReason, UserContent};

use super::{AgentSession, AgentSessionEvent, lock};
use crate::messages::{CustomMessage, CustomRole};

/// Text parts of user content joined without separators (upstream `contentText(content, "")`).
pub(crate) fn content_text(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => blocks.iter().filter_map(ContentBlock::as_text).collect(),
    }
}

impl AgentSession {
    pub(crate) fn on_agent_event(&self, event: &AgentEvent) {
        if let AgentEvent::MessageStart { message: Message::User(user) } = event {
            self.dequeue_user_text(&user.content);
        }
        match event {
            AgentEvent::AgentEnd { messages } => {
                let will_retry = self.will_retry_after_agent_end(messages);
                self.emit(&AgentSessionEvent::AgentEnd { messages, will_retry });
            }
            other => self.emit(&AgentSessionEvent::Agent(other)),
        }
        match event {
            AgentEvent::MessageEnd { message } => self.persist_message_end(message),
            AgentEvent::TurnEnd { .. } => {
                {
                    let mut state = lock(&self.state);
                    state.last_tool_results = std::mem::take(&mut state.turn_tool_results);
                }
                self.flush_pending_custom_messages();
            }
            _ => {}
        }
    }

    /// Remove a delivered steering or follow-up message from its display queue.
    fn dequeue_user_text(&self, content: &UserContent) {
        let text = content_text(content);
        let changed = {
            let mut state = lock(&self.state);
            state.overflow_recovery_attempted = false;
            if text.is_empty() {
                false
            } else if let Some(index) = state.steering.iter().position(|queued| *queued == text) {
                state.steering.remove(index);
                true
            } else if let Some(index) = state.follow_up.iter().position(|queued| *queued == text) {
                state.follow_up.remove(index);
                true
            } else {
                false
            }
        };
        if changed {
            self.emit_queue_update();
        }
    }

    pub(crate) fn emit_queue_update(&self) {
        let (steering, follow_up) = {
            let state = lock(&self.state);
            (state.steering.clone(), state.follow_up.clone())
        };
        self.emit(&AgentSessionEvent::QueueUpdate { steering, follow_up });
    }

    fn persist_message_end(&self, message: &Message) {
        let entry_id = match message {
            Message::Custom(custom) if custom.role == CustomMessage::ROLE => {
                let parsed = CustomMessage::from_message(message);
                parsed.and_then(|custom| {
                    let mut session = self.session_manager();
                    let appended = session.append_custom_message_entry(
                        &custom.custom_type,
                        custom.content,
                        custom.display,
                        custom.details,
                    );
                    report_save_error(appended)
                })
            }
            Message::Custom(_) => None,
            _ => report_save_error(self.session_manager().append_message(message)),
        };
        match message {
            Message::Assistant(assistant) => {
                let finished_retry = {
                    let mut state = lock(&self.state);
                    state.last_assistant = Some((assistant.clone(), entry_id));
                    if !matches!(assistant.stop_reason, StopReason::Error | StopReason::Length) {
                        state.overflow_recovery_attempted = false;
                    }
                    (assistant.stop_reason != StopReason::Error && state.retry_attempt > 0)
                        .then(|| std::mem::take(&mut state.retry_attempt))
                };
                if let Some(attempt) = finished_retry {
                    self.emit(&AgentSessionEvent::AutoRetryEnd { success: true, attempt, final_error: None });
                }
            }
            Message::ToolResult(_) => {
                if let Some(id) = entry_id {
                    lock(&self.state).turn_tool_results.push(id);
                }
            }
            _ => {}
        }
    }

    fn will_retry_after_agent_end(&self, messages: &[Message]) -> bool {
        let settings = self.settings().retry_settings();
        {
            let state = lock(&self.state);
            if state.abort_requested || !settings.enabled || state.retry_attempt >= settings.max_retries {
                return false;
            }
        }
        let last = messages.iter().rev().find_map(|message| match message {
            Message::Assistant(assistant) => Some(assistant),
            _ => None,
        });
        last.is_some_and(|message| self.is_retryable_error(message))
    }

    /// Append custom messages queued during a run once the current turn's tool results are in.
    pub(crate) fn flush_pending_custom_messages(&self) {
        let pending = std::mem::take(&mut lock(&self.state).pending_custom);
        for message in pending {
            self.append_custom_message(message);
        }
    }

    pub(crate) fn append_custom_message(&self, message: Message) {
        if let Some(custom) = CustomMessage::from_message(&message) {
            let appended = self.session_manager().append_custom_message_entry(
                &custom.custom_type,
                custom.content,
                custom.display,
                custom.details,
            );
            report_save_error(appended);
        }
        self.refresh_context();
        self.emit(&AgentSessionEvent::Agent(&AgentEvent::MessageStart { message: message.clone() }));
        self.emit(&AgentSessionEvent::Agent(&AgentEvent::MessageEnd { message }));
    }

    /// Persist `!cmd` results that arrived during a run, after it ends.
    pub(crate) fn flush_pending_bash_messages(&self) {
        let pending = std::mem::take(&mut lock(&self.state).pending_bash);
        if pending.is_empty() {
            return;
        }
        {
            let mut session = self.session_manager();
            for message in &pending {
                report_save_error(session.append_message(message));
            }
        }
        self.refresh_context();
    }
}

/// The new entry id, or `None` after printing why the entry could not be written to disk.
pub(crate) fn report_save_error(result: anyhow::Result<String>) -> Option<String> {
    match result {
        Ok(id) => Some(id),
        Err(error) => {
            eprintln!("Warning: could not save session: {error:#}");
            None
        }
    }
}
