//! Manual and automatic compaction (upstream `compact`, `_checkCompaction`, `_runAutoCompaction`,
//! and `_compactBeforeNextAssistantResponse`).

use std::sync::Arc;

use pi_ai::retry::{RetryEvent, RetryPolicy};
use pi_ai::{AssistantMessage, Message, Model};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::auth::request_auth;
use super::{AgentSession, AgentSessionEvent, CompactionReason, SummarizationSource, lock};
use crate::compaction::{
    CompactionCheck, CompactionCheckInput, CompactionPreparation, CompactionResult, CompactionSettings, RetryEventFn,
    SummarizationOptions, check_compaction, estimate_tokens, exceeds_compaction_threshold, find_message_entry_id,
    prepare_compaction, summarize_compaction,
};
use crate::session::NewCompaction;
use crate::virtual_models::is_virtual_model;

/// Upstream `CompactionPreparation` JSON.
fn preparation_json(preparation: &CompactionPreparation) -> serde_json::Value {
    let ops = &preparation.file_ops;
    json!({
        "firstKeptEntryId": preparation.first_kept_entry_id,
        "messagesToSummarize": preparation.messages_to_summarize,
        "turnPrefixMessages": preparation.turn_prefix_messages,
        "isSplitTurn": preparation.is_split_turn,
        "tokensBefore": preparation.tokens_before,
        "previousSummary": preparation.previous_summary,
        "fileOps": {"read": ops.read, "written": ops.written, "edited": ops.edited},
        "settings": {
            "enabled": preparation.settings.enabled,
            "reserveTokens": preparation.settings.reserve_tokens,
            "keepRecentTokens": preparation.settings.keep_recent_tokens,
        },
    })
}

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

    /// The model whose limits apply to `message`, or `None` when another model produced it.
    /// Under a virtual selection, that is the physical model that produced it.
    pub(crate) fn model_for_message(&self, message: &AssistantMessage) -> Option<Model> {
        let model = self.model()?;
        if is_virtual_model(&model) {
            return self.model_runtime().physical_model(&message.provider, &message.model);
        }
        (model.provider == message.provider && model.id == message.model).then_some(model)
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

    /// The model of a summary (a virtual selection routed first) and its request settings.
    /// Missing auth fails unless the session was created with its own stream function.
    pub(crate) async fn summarization_options(
        &self,
        model: &Model,
        cancel: CancellationToken,
        source: SummarizationSource,
    ) -> anyhow::Result<(Model, SummarizationOptions)> {
        let (model, thinking_level) = self.summary_model(model, &cancel).await?;
        if let Err(message) = request_auth(self.model_runtime(), &model).await
            && !self.has_custom_stream_fn()
        {
            anyhow::bail!(message);
        }
        let options = SummarizationOptions {
            cancel,
            thinking_level: Some(thinking_level),
            stream_fn: Some(self.stream_fn().clone()),
            retry: Some(self.retry_policy()),
            on_retry: Some(self.summarization_retry_callback(source)),
            ..SummarizationOptions::default()
        };
        Ok((model, options))
    }

    /// `session_before_compact` handlers, then pi's summary unless one supplied a compaction,
    /// then `session_compact`. Sets `from_extension` when a handler supplied the summary.
    async fn summarize(
        &self,
        preparation: &CompactionPreparation,
        model: &Model,
        instructions: Option<&str>,
        cancel: CancellationToken,
        reason: CompactionReason,
        will_retry: bool,
        from_extension: &mut bool,
    ) -> anyhow::Result<CompactionResult> {
        let hooked = self.before_compact(preparation, instructions, reason, will_retry, &cancel).await?;
        *from_extension = hooked.is_some();
        let mut result = match hooked {
            Some(result) => result,
            None => {
                let (model, options) =
                    self.summarization_options(model, cancel.clone(), SummarizationSource::Compaction(reason)).await?;
                summarize_compaction(preparation, &model, instructions, &options).await?
            }
        };
        if cancel.is_cancelled() {
            anyhow::bail!("Compaction cancelled");
        }
        let entry = {
            let mut session = self.session_manager();
            let id = session.append_compaction_with(NewCompaction {
                summary: result.summary.clone(),
                first_kept_entry_id: Some(result.first_kept_entry_id.clone()),
                tokens_before: result.tokens_before,
                details: result.details.clone(),
                from_hook: Some(*from_extension),
                usage: Some(result.usage),
            })?;
            let messages = session.build_session_projection().messages;
            result.estimated_tokens_after = Some(messages.iter().map(estimate_tokens).sum());
            session.get_entry(&id).map(|entry| serde_json::Value::Object(entry.raw().clone()))
        };
        self.refresh_context();
        if let (Some(runner), Some(entry)) = (self.extension_runner(), entry) {
            let event = json!({
                "type": "session_compact",
                "compactionEntry": entry,
                "fromExtension": *from_extension,
                "reason": reason.as_str(),
                "willRetry": will_retry,
            });
            runner.emit(event).await;
        }
        Ok(result)
    }

    /// `session_before_compact`: an error when a handler cancelled, a compaction one supplied.
    async fn before_compact(
        &self,
        preparation: &CompactionPreparation,
        instructions: Option<&str>,
        reason: CompactionReason,
        will_retry: bool,
        cancel: &CancellationToken,
    ) -> anyhow::Result<Option<CompactionResult>> {
        let Some(runner) = self.extension_runner().filter(|runner| runner.has_handlers("session_before_compact"))
        else {
            return Ok(None);
        };
        let branch: Vec<serde_json::Value> = {
            let session = self.session_manager();
            session.get_branch(None).into_iter().map(|entry| serde_json::Value::Object(entry.raw().clone())).collect()
        };
        let signal = runner.signal_id(cancel);
        let event = json!({
            "type": "session_before_compact",
            "preparation": preparation_json(preparation),
            "branchEntries": branch,
            "customInstructions": instructions,
            "reason": reason.as_str(),
            "willRetry": will_retry,
            "signal": signal,
        });
        let result = runner.emit(event).await;
        runner.release_signal_id(signal);
        if result["cancel"].as_bool() == Some(true) {
            lock(&self.state).compaction_cancelled_by_extension = true;
            anyhow::bail!("Compaction cancelled");
        }
        let compaction = &result["compaction"];
        let Some(summary) = compaction["summary"].as_str() else { return Ok(None) };
        Ok(Some(CompactionResult {
            summary: summary.to_string(),
            first_kept_entry_id: compaction["firstKeptEntryId"].as_str().unwrap_or_default().to_string(),
            tokens_before: compaction["tokensBefore"].as_u64().unwrap_or(preparation.tokens_before),
            estimated_tokens_after: None,
            usage: serde_json::from_value(compaction["usage"].clone()).unwrap_or_default(),
            details: compaction.get("details").filter(|details| !details.is_null()).cloned(),
        }))
    }

    /// `session_compact_failed` for extensions.
    async fn compact_failed(
        &self,
        reason: CompactionReason,
        error_message: Option<&str>,
        aborted: bool,
        will_retry: bool,
        from_extension: bool,
    ) {
        let Some(runner) = self.extension_runner() else { return };
        let mut event = json!({
            "type": "session_compact_failed",
            "reason": reason.as_str(),
            "aborted": aborted,
            "willRetry": will_retry,
            "fromExtension": from_extension,
        });
        if let Some(message) = error_message {
            event["errorMessage"] = json!(message);
        }
        runner.emit(event).await;
    }

    /// Summarize older history now (`/compact`, RPC `compact`). Aborts the current operation
    /// first and never continues it.
    pub async fn compact(&self, custom_instructions: Option<&str>) -> anyhow::Result<CompactionResult> {
        self.abort().await;
        let cancel = CancellationToken::new();
        lock(&self.state).manual_compaction = Some(cancel.clone());
        self.notify_idle();
        self.emit(&AgentSessionEvent::CompactionStart { reason: CompactionReason::Manual });
        lock(&self.state).compaction_cancelled_by_extension = false;
        let mut from_extension = false;
        let result = self.run_manual_compaction(custom_instructions, cancel.clone(), &mut from_extension).await;
        lock(&self.state).manual_compaction = None;
        self.notify_idle();
        let (result, aborted, error_message) = match result {
            Ok(result) => (Ok(result), false, None),
            Err(error) => {
                let aborted = cancel.is_cancelled() || lock(&self.state).compaction_cancelled_by_extension;
                let message = (!aborted).then(|| format!("Compaction failed: {error}"));
                (Err(error), aborted, message)
            }
        };
        self.emit(&AgentSessionEvent::CompactionEnd {
            reason: CompactionReason::Manual,
            result: result.as_ref().ok(),
            aborted,
            will_retry: false,
            error_message: error_message.clone(),
        });
        if result.is_err() {
            self.compact_failed(CompactionReason::Manual, error_message.as_deref(), aborted, false, from_extension)
                .await;
        }
        result
    }

    async fn run_manual_compaction(
        &self,
        instructions: Option<&str>,
        cancel: CancellationToken,
        from_extension: &mut bool,
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
        self.summarize(&preparation, &model, instructions, cancel, CompactionReason::Manual, false, from_extension)
            .await
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
                let retry = self.run_auto_compaction(CompactionReason::Overflow, true).await;
                if retry {
                    lock(&self.state).failed_response = Some(assistant.clone());
                }
                retry
            }
            CompactionCheck::RecoveryExhausted { error_message } => {
                self.emit(&AgentSessionEvent::CompactionEnd {
                    reason: CompactionReason::Overflow,
                    result: None,
                    aborted: false,
                    will_retry: false,
                    error_message: Some(error_message.clone()),
                });
                self.compact_failed(CompactionReason::Overflow, Some(&error_message), false, false, false).await;
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
        lock(&self.state).compaction_cancelled_by_extension = false;
        let mut from_extension = false;
        let result =
            self.summarize(&preparation, &model, None, cancel.clone(), reason, will_retry, &mut from_extension).await;
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
                let aborted = cancel.is_cancelled() || lock(&self.state).compaction_cancelled_by_extension;
                let error_message = (!aborted).then(|| match reason {
                    CompactionReason::Overflow => format!("Context overflow recovery failed: {error}"),
                    _ => format!("Auto-compaction failed: {error}"),
                });
                self.emit(&AgentSessionEvent::CompactionEnd {
                    reason,
                    result: None,
                    aborted,
                    will_retry: false,
                    error_message: error_message.clone(),
                });
                self.compact_failed(reason, error_message.as_deref(), aborted, false, from_extension).await;
                false
            }
        }
    }

    /// Compact between turns of one run when the projected context crosses the threshold.
    pub(crate) async fn compact_before_next_response(&self) {
        // A virtual selection is checked when the request is routed, against the routed model.
        let Some(model) = self.model().filter(|model| !is_virtual_model(model)) else { return };
        let settings = self.compaction_settings(Some(&model));
        let exceeds = exceeds_compaction_threshold(&self.session_manager(), &model, &settings);
        if exceeds {
            self.run_auto_compaction(CompactionReason::Threshold, false).await;
        }
    }
}
