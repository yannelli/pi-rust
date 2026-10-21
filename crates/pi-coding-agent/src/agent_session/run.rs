//! The session-level run (upstream `_runAgentPrompt` and `_handlePostAgentRun`): one agent run,
//! then auto retry of transient errors, overflow recovery, and queued messages until settled.

use std::time::Duration;

use pi_ai::retry::{is_retryable_assistant_error, retry_delay_ms};
use pi_ai::{AssistantMessage, Message, StopReason, is_context_overflow};
use tokio_util::sync::CancellationToken;

use super::{AgentSession, AgentSessionEvent, lock};

impl AgentSession {
    pub(crate) fn abort_requested(&self) -> bool {
        lock(&self.state).abort_requested
    }

    /// Run `messages` and every automatic continuation, then emit `agent_settled`. The run is
    /// already marked active by [`Self::start_run`].
    pub(crate) async fn run_agent_prompt(&self, messages: Vec<Message>) -> anyhow::Result<()> {
        // The run records the loadout; restored tools that did not register by now are dropped.
        lock(&self.tools).pending.clear();
        // Compaction before the prompt may have scheduled a retry; the new prompt replaces it.
        lock(&self.state).failed_response = None;
        self.record_selection();
        let result = self.drive_run(messages).await;
        if self.abort_requested() {
            self.finish_cancelled_retry();
        }
        {
            let mut state = lock(&self.state);
            state.run_prompt = None;
            state.failed_response = None;
        }
        self.flush_pending_bash_messages();
        self.flush_pending_custom_messages();
        self.emit_agent_settled().await;
        result
    }

    async fn drive_run(&self, messages: Vec<Message>) -> anyhow::Result<()> {
        self.agent.prompt_messages(messages).await?;
        while !self.abort_requested() {
            if self.handle_post_agent_run().await? {
                if self.abort_requested() {
                    break;
                }
                self.agent.continue_run().await?;
                continue;
            }
            if self.abort_requested() || !self.before_settle_boundary().await || self.abort_requested() {
                break;
            }
            self.agent.continue_run().await?;
        }
        Ok(())
    }

    async fn emit_agent_settled(&self) {
        let aborted = {
            let mut state = lock(&self.state);
            state.run_active = false;
            state.abort_requested
        };
        self.cache_warmer().on_agent_settled();
        if let Some(runner) = self.extension_runner() {
            runner.emit(serde_json::json!({"type": "agent_settled", "aborted": aborted})).await;
        }
        self.emit(&AgentSessionEvent::AgentSettled { aborted });
        self.notify_idle();
    }

    /// Whether the run continues: a retry, overflow recovery, or queued messages.
    async fn handle_post_agent_run(&self) -> anyhow::Result<bool> {
        let (last, tool_results) = {
            let mut state = lock(&self.state);
            (state.last_assistant.take(), std::mem::take(&mut state.last_tool_results))
        };
        if self.abort_requested() {
            self.finish_cancelled_retry();
            return Ok(false);
        }
        let Some((message, entry_id)) = last else { return Ok(self.agent.has_queued_messages()) };
        if self.is_retryable_error(&message) && self.prepare_retry(&message, entry_id.as_deref()).await {
            if self.abort_requested() {
                self.finish_cancelled_retry();
            }
            lock(&self.state).failed_response = Some(message);
            return Ok(!self.abort_requested());
        }
        if self.abort_requested() {
            self.finish_cancelled_retry();
            return Ok(false);
        }
        if message.stop_reason == StopReason::Error {
            let attempt = std::mem::take(&mut lock(&self.state).retry_attempt);
            if attempt > 0 {
                let final_error = message.error_message.clone();
                self.emit(&AgentSessionEvent::AutoRetryEnd { success: false, attempt, final_error });
            }
        }
        if self.check_compaction(&message, entry_id.as_deref(), true, &tool_results).await {
            return Ok(!self.abort_requested());
        }
        Ok(!self.abort_requested() && self.agent.has_queued_messages())
    }

    /// Transient errors are retried; context overflow is left to compaction.
    pub(crate) fn is_retryable_error(&self, message: &AssistantMessage) -> bool {
        let window = self.limits_model_for(message).map_or(0, |model| model.context_window);
        !is_context_overflow(message, Some(window)) && is_retryable_assistant_error(message)
    }

    fn finish_cancelled_retry(&self) {
        let attempt = std::mem::take(&mut lock(&self.state).retry_attempt);
        if attempt > 0 {
            self.emit(&AgentSessionEvent::AutoRetryEnd {
                success: false,
                attempt,
                final_error: Some("Retry cancelled".into()),
            });
        }
    }

    /// Back off before retrying; false when retries are off, used up, or cancelled.
    async fn prepare_retry(&self, message: &AssistantMessage, entry_id: Option<&str>) -> bool {
        let settings = self.settings().retry_settings();
        if !settings.enabled {
            return false;
        }
        let (attempt, sleep) = {
            let mut state = lock(&self.state);
            if state.retry_attempt >= settings.max_retries {
                return false;
            }
            state.retry_attempt += 1;
            let sleep = CancellationToken::new();
            state.retry_sleep = Some(sleep.clone());
            (state.retry_attempt, sleep)
        };
        let delay_ms = retry_delay_ms(settings.base_delay_ms, Some(settings.max_agent_delay_ms), attempt);
        let error_message =
            message.error_message.clone().filter(|error| !error.is_empty()).unwrap_or_else(|| "Unknown error".into());
        self.emit(&AgentSessionEvent::AutoRetryStart {
            attempt,
            max_attempts: settings.max_retries,
            delay_ms,
            error_message,
        });
        self.omit_recovery_attempt(entry_id.into_iter().map(String::from).collect());
        let cancelled = tokio::select! {
            _ = sleep.cancelled() => true,
            _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => false,
        };
        lock(&self.state).retry_sleep = None;
        if cancelled {
            self.finish_cancelled_retry();
        }
        !cancelled
    }

    /// Hide a failed response (and its tool results) from future context with `context_edit`
    /// entries, keeping it in the raw history, then refresh the transcript.
    pub(crate) fn omit_recovery_attempt(&self, targets: Vec<String>) {
        let mut appended = Vec::new();
        {
            let mut session = self.session_manager();
            for target in targets {
                match session.append_context_edit(&target, None) {
                    Ok(id) => appended.push(id),
                    Err(error) => eprintln!("Warning: could not omit failed response: {error:#}"),
                }
            }
        }
        for id in &appended {
            self.emit_entry(id);
        }
        self.refresh_context();
    }

    /// Cancel a pending retry without stopping a running request.
    pub fn abort_retry(&self) {
        if let Some(sleep) = &lock(&self.state).retry_sleep {
            sleep.cancel();
        }
    }

    pub fn is_retrying(&self) -> bool {
        lock(&self.state).retry_sleep.is_some()
    }

    /// The current retry number (0 when not retrying).
    pub fn retry_attempt(&self) -> u32 {
        lock(&self.state).retry_attempt
    }

    pub fn auto_retry_enabled(&self) -> bool {
        self.settings().retry_enabled()
    }

    pub fn set_auto_retry_enabled(&self, enabled: bool) {
        self.settings().set_retry_enabled(enabled);
    }
}
