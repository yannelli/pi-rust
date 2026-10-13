//! Manual and automatic compaction (upstream `compact`, `_checkCompaction`, `_runAutoCompaction`,
//! and `_compactBeforeNextAssistantResponse`).

use std::sync::Arc;

use pi_ai::retry::{RetryEvent, RetryPolicy};
use pi_ai::{AssistantMessage, Message, Model};
use tokio_util::sync::CancellationToken;

use super::auth::request_auth;
use super::{AgentSession, AgentSessionEvent, CompactionReason, SummarizationSource, lock};
use crate::compaction::{
    CompactionCheck, CompactionCheckInput, CompactionPreparation, CompactionResult, CompactionSettings, RetryEventFn,
    SummarizationOptions, check_compaction, estimate_tokens, exceeds_compaction_threshold, find_message_entry_id,
    prepare_compaction, summarize_compaction,
};
use crate::session::NewCompaction;

pub(crate) fn no_model_selected_message() -> String {
    format!("No model selected. {}", crate::app::provider_login_help())
}

impl AgentSession {
    pub(crate) fn compaction_settings(&self, model: Option<&Model>) -> CompactionSettings {
        let config = self.settings().compaction_settings(model);
        CompactionSettings {
            enabled: config.enabled,
            reserve_tokens: config.reserve_tokens,
            keep_recent_tokens: config.keep_recent_tokens,
        }
    }

    pub(crate) fn retry_policy(&self) -> RetryPolicy {
        let retry = self.settings().retry_settings();
        RetryPolicy {
            enabled: retry.enabled,
            max_retries: retry.max_retries,
            base_delay_ms: retry.base_delay_ms,
            max_agent_delay_ms: Some(retry.max_agent_delay_ms),
        }
    }

    /// The model that produced `message` when it is the current one; `None` for another model.
    pub(crate) fn model_for_message(&self, message: &AssistantMessage) -> Option<Model> {
        self.model().filter(|model| model.provider == message.provider && model.id == message.model)
    }

    /// The model whose limits apply to `message`.
    pub(crate) fn limits_model_for(&self, message: &AssistantMessage) -> Option<Model> {
        self.model_for_message(message).or_else(|| self.model())
    }

    pub(crate) fn summarization_retry_callback(&self, source: SummarizationSource) -> RetryEventFn {
        let weak = self.weak.clone();
        Arc::new(move |event| {
            let Some(session) = weak.upgrade() else { return };
            match event {
                RetryEvent::Scheduled { retry, max_retries, delay_ms, error_message } => {
                    session.emit(&AgentSessionEvent::SummarizationRetryScheduled {
                        attempt: retry,
                        max_attempts: max_retries,
                        delay_ms,
                        error_message,
                    })
                }
                RetryEvent::RetryStart => session.emit(&AgentSessionEvent::SummarizationRetryAttemptStart { source }),
                RetryEvent::Finished { .. } => session.emit(&AgentSessionEvent::SummarizationRetryFinished),
            }
        })
    }

    /// Request settings for a summary with `model`. Missing auth fails unless the session was
    /// created with its own stream function.
    pub(crate) async fn summarization_options(
        &self,
        model: &Model,
        cancel: CancellationToken,
        source: SummarizationSource,
    ) -> anyhow::Result<SummarizationOptions> {
        if let Err(message) = request_auth(self.model_runtime(), model).await
            && !self.has_custom_stream_fn()
        {
            anyhow::bail!(message);
        }
        Ok(SummarizationOptions {
            cancel,
            thinking_level: Some(self.thinking_level()),
            stream_fn: Some(self.stream_fn().clone()),
            retry: Some(self.retry_policy()),
            on_retry: Some(self.summarization_retry_callback(source)),
            ..SummarizationOptions::default()
        })
    }

    async fn summarize(
        &self,
        preparation: &CompactionPreparation,
        model: &Model,
        instructions: Option<&str>,
        cancel: CancellationToken,
        reason: CompactionReason,
    ) -> anyhow::Result<CompactionResult> {
        let options =
            self.summarization_options(model, cancel.clone(), SummarizationSource::Compaction(reason)).await?;
        let mut result = summarize_compaction(preparation, model, instructions, &options).await?;
        if cancel.is_cancelled() {
            anyhow::bail!("Compaction cancelled");
        }
        {
            let mut session = self.session_manager();
            session.append_compaction_with(NewCompaction {
                summary: result.summary.clone(),
                first_kept_entry_id: Some(result.first_kept_entry_id.clone()),
                tokens_before: result.tokens_before,
                details: result.details.clone(),
                from_hook: Some(false),
                usage: Some(result.usage),
            })?;
            let messages = session.build_session_projection().messages;
            result.estimated_tokens_after = Some(messages.iter().map(estimate_tokens).sum());
        }
        self.refresh_context();
        Ok(result)
    }

    /// Summarize older history now (`/compact`, RPC `compact`). Aborts the current operation
    /// first and never continues it.
    pub async fn compact(&self, custom_instructions: Option<&str>) -> anyhow::Result<CompactionResult> {
        self.abort().await;
        let cancel = CancellationToken::new();
        lock(&self.state).manual_compaction = Some(cancel.clone());
        self.notify_idle();
        self.emit(&AgentSessionEvent::CompactionStart { reason: CompactionReason::Manual });
        let result = self.run_manual_compaction(custom_instructions, cancel.clone()).await;
        lock(&self.state).manual_compaction = None;
        self.notify_idle();
        let (result, aborted, error_message) = match result {
            Ok(result) => (Ok(result), false, None),
            Err(error) => {
                let aborted = cancel.is_cancelled();
                let message = (!aborted).then(|| format!("Compaction failed: {error}"));
                (Err(error), aborted, message)
            }
        };
        self.emit(&AgentSessionEvent::CompactionEnd {
            reason: CompactionReason::Manual,
            result: result.as_ref().ok(),
            aborted,
            will_retry: false,
            error_message,
        });
        result
    }

    async fn run_manual_compaction(
        &self,
        instructions: Option<&str>,
        cancel: CancellationToken,
    ) -> anyhow::Result<CompactionResult> {
        let model = self.model().ok_or_else(|| anyhow::anyhow!(no_model_selected_message()))?;
        let settings = self.compaction_settings(Some(&model));
        let preparation = {
            let session = self.session_manager();
            match prepare_compaction(&session, &settings) {
                Some(preparation) => preparation,
                None if session.leaf_entry().is_some_and(|entry| entry.compaction().is_some()) => {
                    anyhow::bail!("Already compacted")
                }
                None => anyhow::bail!("Nothing to compact (session too small)"),
            }
        };
        self.summarize(&preparation, &model, instructions, cancel, CompactionReason::Manual).await
    }

    /// Cancel a running manual or automatic compaction.
    pub fn abort_compaction(&self) {
        let state = lock(&self.state);
        for token in [&state.manual_compaction, &state.auto_compaction].into_iter().flatten() {
            token.cancel();
        }
    }

    pub fn auto_compaction_enabled(&self) -> bool {
        self.settings().compaction_enabled()
    }

    pub fn set_auto_compaction_enabled(&self, enabled: bool) {
        self.settings().set_compaction_enabled(enabled);
    }

    /// Decide and run automatic compaction after a response. Returns whether the run should
    /// continue (overflow recovery, or queued messages after a compaction).
    pub(crate) async fn check_compaction(
        &self,
        assistant: &AssistantMessage,
        entry_id: Option<&str>,
        skip_aborted: bool,
        tool_results: &[String],
    ) -> bool {
        let Some(current) = self.model() else { return false };
        let settings = self.compaction_settings(Some(&current));
        if !settings.enabled {
            return false;
        }
        let message_model = self.model_for_message(assistant);
        let agent_messages = self.messages();
        let (check, entry_id) = {
            let session = self.session_manager();
            let message = Message::Assistant(assistant.clone());
            let entry_id = entry_id.map(String::from).or_else(|| find_message_entry_id(&session, &message));
            let recovery_attempted = lock(&self.state).overflow_recovery_attempted;
            let input = CompactionCheckInput {
                assistant,
                assistant_entry_id: entry_id.as_deref(),
                message_model: message_model.as_ref(),
                current_model: &current,
                settings: &settings,
                skip_aborted,
                recovery_attempted,
                agent_messages: &agent_messages,
            };
            (check_compaction(&session, input), entry_id)
        };
        match check {
            CompactionCheck::Nothing => false,
            CompactionCheck::Threshold => self.run_auto_compaction(CompactionReason::Threshold, false).await,
            CompactionCheck::Overflow { will_retry: false } => {
                self.run_auto_compaction(CompactionReason::Overflow, false).await
            }
            CompactionCheck::Overflow { will_retry: true } => {
                lock(&self.state).overflow_recovery_attempted = true;
                let targets = entry_id.into_iter().chain(tool_results.iter().cloned()).collect();
                self.omit_recovery_attempt(targets);
                self.run_auto_compaction(CompactionReason::Overflow, true).await
            }
            CompactionCheck::RecoveryExhausted { error_message } => {
                self.emit(&AgentSessionEvent::CompactionEnd {
                    reason: CompactionReason::Overflow,
                    result: None,
                    aborted: false,
                    will_retry: false,
                    error_message: Some(error_message),
                });
                false
            }
        }
    }

    /// Threshold or overflow compaction. Returns whether the run should continue.
    pub(crate) async fn run_auto_compaction(&self, reason: CompactionReason, will_retry: bool) -> bool {
        let Some(model) = self.model() else { return false };
        let settings = self.compaction_settings(Some(&model));
        let Some(preparation) = prepare_compaction(&self.session_manager(), &settings) else { return false };
        let cancel = CancellationToken::new();
        lock(&self.state).auto_compaction = Some(cancel.clone());
        self.notify_idle();
        self.emit(&AgentSessionEvent::CompactionStart { reason });
        let result = self.summarize(&preparation, &model, None, cancel.clone(), reason).await;
        lock(&self.state).auto_compaction = None;
        self.notify_idle();
        match result {
            Ok(result) => {
                self.emit(&AgentSessionEvent::CompactionEnd {
                    reason,
                    result: Some(&result),
                    aborted: false,
                    will_retry,
                    error_message: None,
                });
                will_retry || self.agent.has_queued_messages()
            }
            Err(error) => {
                let aborted = cancel.is_cancelled();
                let error_message = (!aborted).then(|| match reason {
                    CompactionReason::Overflow => format!("Context overflow recovery failed: {error}"),
                    _ => format!("Auto-compaction failed: {error}"),
                });
                self.emit(&AgentSessionEvent::CompactionEnd {
                    reason,
                    result: None,
                    aborted,
                    will_retry: false,
                    error_message,
                });
                false
            }
        }
    }

    /// Compact between turns of one run when the projected context crosses the threshold.
    pub(crate) async fn compact_before_next_response(&self) {
        let Some(model) = self.model() else { return };
        let settings = self.compaction_settings(Some(&model));
        let exceeds = exceeds_compaction_threshold(&self.session_manager(), &model, &settings);
        if exceeds {
            self.run_auto_compaction(CompactionReason::Threshold, false).await;
        }
    }
}
