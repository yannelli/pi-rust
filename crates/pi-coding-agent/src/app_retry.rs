//! Agent-level auto retry (upstream `agent-session.ts` and `ai/utils/retry.ts`): which
//! provider errors are transient, and the exponential backoff between tries.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use pi_agent::AgentEvent;
use pi_ai::{AssistantMessage, Message, StopReason, is_context_overflow};
use regex::Regex;
use tokio_util::sync::CancellationToken;

use super::{CodingSession, SessionEvent, SessionListener, lock};
use crate::settings::SettingsManager;

fn alternation(patterns: &[&str]) -> Regex {
    Regex::new(&format!("(?i){}", patterns.join("|"))).expect("retry patterns are valid")
}

/// Account limits and billing failures that waiting does not fix.
static NON_RETRYABLE: LazyLock<Regex> = LazyLock::new(|| {
    alternation(&[
        "GoUsageLimitError",
        "FreeUsageLimitError",
        "Monthly usage limit reached",
        "available balance",
        "insufficient_quota",
        "out of budget",
        "quota exceeded",
        "billing",
        "subscription_sharing_usage_limit_exceeded",
    ])
});

/// Overload, rate limit, server, network, and early stream end errors.
static RETRYABLE: LazyLock<Regex> = LazyLock::new(|| {
    alternation(&[
        "overloaded",
        "server_busy",
        "servers are currently busy",
        "currently experiencing high demand",
        "model is at capacity",
        "rate.?limit",
        "too many requests",
        "429",
        "500",
        "502",
        "503",
        "504",
        "520",
        "524",
        "service.?unavailable",
        "server.?error",
        "internal.?error",
        "provider.?returned.?error",
        "exceeded request buffer limit while retrying upstream",
        "network.?error",
        "connection.?error",
        "connection.?refused",
        "connection.?lost",
        "other side closed",
        "fetch failed",
        "getaddrinfo",
        "ENOTFOUND",
        "EAI_AGAIN",
        "upstream.?connect",
        "reset before headers",
        "socket hang up",
        "socket connection was closed",
        "timed? out",
        "timeout",
        "terminated",
        "websocket.?closed",
        "websocket.?error",
        "ended without",
        "stream ended before message_stop",
        "stream ended before a terminal response event",
        "http2 request did not get a response",
        "pending stream has been canceled",
        "retry delay",
        "you can retry your request",
        "try your request again",
        "please retry your request",
        "ResourceExhausted",
        "subscription_sharing_usage_unavailable",
        "subscription_sharing_user_unavailable",
    ])
});

/// Whether an assistant message failed with a transient provider error (upstream
/// `isRetryableAssistantError`). Context overflow is handled by compaction instead.
pub fn is_retryable_assistant_error(message: &AssistantMessage) -> bool {
    if message.stop_reason != StopReason::Error {
        return false;
    }
    let Some(error) = message.error_message.as_deref().filter(|error| !error.is_empty()) else { return false };
    !NON_RETRYABLE.is_match(error) && RETRYABLE.is_match(error)
}

/// Delay before retry number `n`: `base * 2^(n - 1)`, capped at `max_delay_ms`.
pub fn retry_delay_ms(base_delay_ms: u64, max_delay_ms: u64, n: u32) -> u64 {
    let factor = 1u64.checked_shl(n.saturating_sub(1)).unwrap_or(u64::MAX);
    base_delay_ms.saturating_mul(factor).min(max_delay_ms)
}

/// Retry progress shared between the session and its agent event listener.
#[derive(Default)]
pub(super) struct RetryState {
    /// Retries made for the current failure; 0 when no retry is in progress.
    pub count: u32,
    /// Cancels the backoff sleep.
    pub sleep: Option<CancellationToken>,
    /// Set by `CodingSession::abort` until the next prompt.
    pub abort_requested: bool,
    /// The latest assistant message of the current run.
    pub last_assistant: Option<AssistantMessage>,
}

/// State shared between the session and its agent event listener.
pub(super) struct Shared {
    pub settings: Arc<SettingsManager>,
    pub listeners: Mutex<Vec<(u64, SessionListener)>>,
    pub next_listener: AtomicU64,
    pub retry: Mutex<RetryState>,
    pub context_window: AtomicU64,
}

impl Shared {
    pub fn new(settings: Arc<SettingsManager>, context_window: u64) -> Self {
        Self {
            settings,
            listeners: Mutex::default(),
            next_listener: AtomicU64::new(1),
            retry: Mutex::default(),
            context_window: AtomicU64::new(context_window),
        }
    }

    pub fn emit(&self, event: &SessionEvent<'_>) {
        let listeners: Vec<SessionListener> =
            lock(&self.listeners).iter().map(|(_, listener)| listener.clone()).collect();
        for listener in listeners {
            listener(event);
        }
    }

    /// Retryable unless it is a context overflow, which compaction handles.
    pub fn is_retryable(&self, message: &AssistantMessage) -> bool {
        !is_context_overflow(message, Some(self.context_window.load(Ordering::Relaxed)))
            && is_retryable_assistant_error(message)
    }

    fn will_retry(&self, messages: &[Message]) -> bool {
        let settings = self.settings.retry_settings();
        {
            let retry = lock(&self.retry);
            if retry.abort_requested || !settings.enabled || retry.count >= settings.max_retries {
                return false;
            }
        }
        let last = messages.iter().rev().find_map(|message| match message {
            Message::Assistant(assistant) => Some(assistant),
            _ => None,
        });
        last.is_some_and(|message| self.is_retryable(message))
    }

    /// Forward an agent event to listeners and track the run's last assistant message.
    pub fn on_agent_event(&self, event: &AgentEvent) {
        match event {
            AgentEvent::AgentEnd { messages } => {
                self.emit(&SessionEvent::AgentEnd { messages, will_retry: self.will_retry(messages) })
            }
            other => self.emit(&SessionEvent::Agent(other)),
        }
        if let AgentEvent::MessageEnd { message: Message::Assistant(assistant) } = event {
            let finished = {
                let mut retry = lock(&self.retry);
                retry.last_assistant = Some(assistant.clone());
                (assistant.stop_reason != StopReason::Error).then(|| std::mem::take(&mut retry.count))
            };
            if let Some(attempt) = finished.filter(|attempt| *attempt > 0) {
                self.emit(&SessionEvent::AutoRetryEnd { success: true, attempt, final_error: None });
            }
        }
    }

    pub fn finish_retry(&self, final_error: &str) {
        let attempt = std::mem::take(&mut lock(&self.retry).count);
        if attempt > 0 {
            self.emit(&SessionEvent::AutoRetryEnd { success: false, attempt, final_error: Some(final_error) });
        }
    }
}

impl CodingSession {
    /// Retry transient errors and recover from context overflow after a run (upstream
    /// `_handlePostAgentRun`).
    pub(super) async fn after_run(&self) -> anyhow::Result<()> {
        let mut overflow_compacted = false;
        loop {
            let (aborted, message) = {
                let mut retry = lock(&self.shared.retry);
                (retry.abort_requested, retry.last_assistant.take())
            };
            if aborted {
                self.shared.finish_retry("Retry cancelled");
                return Ok(());
            }
            let Some(message) = message else { return Ok(()) };
            if self.shared.is_retryable(&message) && self.prepare_retry(&message).await {
                if lock(&self.shared.retry).abort_requested {
                    self.shared.finish_retry("Retry cancelled");
                    return Ok(());
                }
                self.drop_failed_response();
                self.agent.continue_run().await?;
                continue;
            }
            if message.stop_reason == StopReason::Error {
                self.shared.finish_retry(message.error_message.as_deref().unwrap_or_default());
            }
            let window = self.model().context_window;
            if !overflow_compacted && self.compaction.enabled && is_context_overflow(&message, Some(window)) {
                overflow_compacted = true;
                self.drop_failed_response();
                if self.compact(None).await.is_ok() {
                    self.agent.continue_run().await?;
                    continue;
                }
            }
            return Ok(());
        }
    }

    /// Wait out the backoff for the next retry. False when retries are off, used up, or cancelled.
    async fn prepare_retry(&self, message: &AssistantMessage) -> bool {
        let settings = self.settings.retry_settings();
        if !settings.enabled {
            return false;
        }
        let (count, sleep) = {
            let mut retry = lock(&self.shared.retry);
            if retry.count >= settings.max_retries {
                return false;
            }
            retry.count += 1;
            let sleep = CancellationToken::new();
            retry.sleep = Some(sleep.clone());
            (retry.count, sleep)
        };
        let delay_ms = retry_delay_ms(settings.base_delay_ms, settings.max_agent_delay_ms, count);
        let error_message =
            message.error_message.as_deref().filter(|error| !error.is_empty()).unwrap_or("Unknown error");
        self.shared.emit(&SessionEvent::AutoRetryStart {
            attempt: count,
            max_attempts: settings.max_retries,
            delay_ms,
            error_message,
        });
        let cancelled = tokio::select! {
            _ = sleep.cancelled() => true,
            _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => false,
        };
        lock(&self.shared.retry).sleep = None;
        if cancelled {
            self.shared.finish_retry("Retry cancelled");
        }
        !cancelled
    }

    /// Remove a failed final response from the context before continuing.
    fn drop_failed_response(&self) {
        self.agent.update_state(|state| {
            if matches!(state.messages.last(), Some(Message::Assistant(last)) if last.stop_reason != StopReason::Stop) {
                state.messages.pop();
            }
        });
    }

    /// Stop the current run and any pending retry.
    pub fn abort(&self) {
        {
            let mut retry = lock(&self.shared.retry);
            retry.abort_requested = true;
            if let Some(sleep) = &retry.sleep {
                sleep.cancel();
            }
        }
        self.agent.abort();
    }

    /// Cancel a pending retry without stopping a running request.
    pub fn abort_retry(&self) {
        if let Some(sleep) = &lock(&self.shared.retry).sleep {
            sleep.cancel();
        }
    }

    pub fn is_retrying(&self) -> bool {
        lock(&self.shared.retry).sleep.is_some()
    }

    pub fn auto_retry_enabled(&self) -> bool {
        self.settings.retry_enabled()
    }

    pub fn set_auto_retry_enabled(&self, enabled: bool) {
        self.settings.set_retry_enabled(enabled);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed(error: &str) -> AssistantMessage {
        AssistantMessage {
            stop_reason: StopReason::Error,
            error_message: Some(error.into()),
            ..AssistantMessage::empty(&pi_ai::Model::default())
        }
    }

    #[test]
    fn classifies_provider_errors_like_upstream() {
        for error in ["529 overloaded_error", "Rate limit reached", "HTTP 503", "fetch failed", "Request timed out"] {
            assert!(is_retryable_assistant_error(&failed(error)), "{error}");
        }
        for error in ["insufficient_quota (429)", "invalid api key", "Monthly usage limit reached: 503"] {
            assert!(!is_retryable_assistant_error(&failed(error)), "{error}");
        }
        let mut stopped = failed("503");
        stopped.stop_reason = StopReason::Stop;
        assert!(!is_retryable_assistant_error(&stopped));
    }

    #[test]
    fn backs_off_exponentially_with_a_cap() {
        assert_eq!([1, 2, 3, 4].map(|n| retry_delay_ms(2000, 60_000, n)), [2000, 4000, 8000, 16000]);
        assert_eq!(retry_delay_ms(2000, 5000, 3), 5000);
        assert_eq!(retry_delay_ms(u64::MAX / 2, u64::MAX, 70), u64::MAX);
    }
}
