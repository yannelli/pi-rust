//! Bounded retry of one summarization call on transient provider errors, with callbacks for
//! retry events. Port of upstream `utils/retry.ts` (`retryAssistantCall`).

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use futures::future::BoxFuture;
use pi_ai::{AssistantMessage, StopReason};
use regex::Regex;
use tokio_util::sync::CancellationToken;

pub const DEFAULT_MAX_AGENT_RETRY_DELAY_MS: u64 = 60_000;

/// Exponential backoff: `base_delay_ms * 2^(n-1)` for retry `n`, capped by `max_agent_delay_ms`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    pub enabled: bool,
    /// Retries after the first call (0 disables retrying).
    pub max_retries: u32,
    pub base_delay_ms: u64,
    pub max_agent_delay_ms: Option<u64>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self { enabled: true, max_retries: 3, base_delay_ms: 2000, max_agent_delay_ms: None }
    }
}

pub fn retry_delay_ms(policy: &RetryPolicy, retry: u32) -> u64 {
    let factor = 2u64.checked_pow(retry.saturating_sub(1)).unwrap_or(u64::MAX);
    let cap = policy.max_agent_delay_ms.unwrap_or(DEFAULT_MAX_AGENT_RETRY_DELAY_MS);
    policy.base_delay_ms.saturating_mul(factor).min(cap)
}

pub type RetryScheduledFn = Arc<dyn Fn(u32, u32, u64, &str) + Send + Sync>;
pub type RetryAttemptStartFn = Arc<dyn Fn() + Send + Sync>;
pub type RetryFinishedFn = Arc<dyn Fn(bool, u32, Option<&str>) + Send + Sync>;

/// Retry notifications, for example the app's `summarization_retry_*` events.
#[derive(Clone, Default)]
pub struct RetryCallbacks {
    /// `(retry number, max retries, delay ms, error message)` before each backoff sleep.
    pub on_retry_scheduled: Option<RetryScheduledFn>,
    /// After the sleep, just before the retried call.
    pub on_retry_attempt_start: Option<RetryAttemptStartFn>,
    /// `(success, retry number, final error)` once, if any retry was scheduled.
    pub on_retry_finished: Option<RetryFinishedFn>,
}

static NON_RETRYABLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        "(?i)GoUsageLimitError|FreeUsageLimitError|Monthly usage limit reached|available balance|insufficient_quota\
         |out of budget|quota exceeded|billing|subscription_sharing_usage_limit_exceeded",
    )
    .expect("valid pattern")
});

static RETRYABLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        "(?i)overloaded|server_busy|servers are currently busy|currently experiencing high demand|model is at capacity\
         |rate.?limit|too many requests|429|500|502|503|504|520|524|service.?unavailable|server.?error|internal.?error\
         |provider.?returned.?error|exceeded request buffer limit while retrying upstream|network.?error\
         |connection.?error|connection.?refused|connection.?lost|other side closed|fetch failed|getaddrinfo\
         |ENOTFOUND|EAI_AGAIN|upstream.?connect|reset before headers|socket hang up|socket connection was closed\
         |timed? out|timeout|terminated|websocket.?closed|websocket.?error|ended without\
         |stream ended before message_stop|stream ended before a terminal response event\
         |http2 request did not get a response|pending stream has been canceled|retry delay\
         |you can retry your request|try your request again|please retry your request|ResourceExhausted\
         |subscription_sharing_usage_unavailable|subscription_sharing_user_unavailable",
    )
    .expect("valid pattern")
});

/// Whether a failed response looks like a transient provider or transport error.
pub fn is_retryable_assistant_error(message: &AssistantMessage) -> bool {
    let Some(error) = message.error_message.as_deref().filter(|_| message.stop_reason == StopReason::Error) else {
        return false;
    };
    !error.is_empty() && !NON_RETRYABLE.is_match(error) && RETRYABLE.is_match(error)
}

/// Run `produce`, retrying transient errors per `policy`. Aborts are never retried; an abort
/// during backoff returns the failed response marked aborted.
pub async fn retry_assistant_call(
    mut produce: impl FnMut() -> BoxFuture<'static, AssistantMessage>,
    policy: Option<&RetryPolicy>,
    cancel: &CancellationToken,
    callbacks: &RetryCallbacks,
) -> AssistantMessage {
    let max_retries = policy.filter(|policy| policy.enabled).map_or(0, |policy| policy.max_retries);
    let finished = |success: bool, retry: u32, error: Option<&str>| {
        if let Some(callback) = &callbacks.on_retry_finished {
            callback(success, retry, error);
        }
    };
    let mut retry = 0;
    let mut last_retry: Option<u32> = None;
    loop {
        let response = produce().await;
        if response.stop_reason == StopReason::Aborted {
            if let Some(retry) = last_retry {
                finished(false, retry, None);
            }
            return response;
        }
        if response.stop_reason != StopReason::Error {
            if let Some(retry) = last_retry {
                finished(true, retry, None);
            }
            return response;
        }
        let Some(policy) = policy.filter(|_| retry < max_retries && is_retryable_assistant_error(&response)) else {
            if let Some(retry) = last_retry {
                finished(false, retry, response.error_message.as_deref());
            }
            return response;
        };
        retry += 1;
        last_retry = Some(retry);
        let error = response.error_message.clone().filter(|error| !error.is_empty()).unwrap_or("Unknown error".into());
        let delay = retry_delay_ms(policy, retry);
        if let Some(callback) = &callbacks.on_retry_scheduled {
            callback(retry, max_retries, delay, &error);
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
            _ = cancel.cancelled() => {
                finished(false, retry, Some(&error));
                return AssistantMessage { stop_reason: StopReason::Aborted, error_message: None, ..response };
            }
        }
        if let Some(callback) = &callbacks.on_retry_attempt_start {
            callback();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    fn failed(error: &str) -> AssistantMessage {
        let mut message = AssistantMessage::empty(&pi_ai::providers::faux::FauxProvider::model());
        message.stop_reason = StopReason::Error;
        message.error_message = Some(error.into());
        message
    }

    #[test]
    fn classifies_errors() {
        assert!(is_retryable_assistant_error(&failed("socket hang up")));
        assert!(is_retryable_assistant_error(&failed("HTTP 503 Service Unavailable")));
        assert!(!is_retryable_assistant_error(&failed("429 insufficient_quota")));
        assert!(!is_retryable_assistant_error(&failed("invalid api key")));
        let policy = RetryPolicy { base_delay_ms: 1000, max_agent_delay_ms: Some(3000), ..RetryPolicy::default() };
        let delays = (retry_delay_ms(&policy, 1), retry_delay_ms(&policy, 2), retry_delay_ms(&policy, 5));
        assert_eq!(delays, (1000, 2000, 3000));
    }

    #[tokio::test]
    async fn retries_transient_errors_and_reports() {
        let responses = Arc::new(Mutex::new(vec![failed("terminated"), failed("terminated")]));
        let events = Arc::new(Mutex::new(Vec::new()));
        let (scheduled, finished) = (events.clone(), events.clone());
        let callbacks = RetryCallbacks {
            on_retry_scheduled: Some(Arc::new(move |retry, max, _, error| {
                scheduled.lock().unwrap().push(format!("scheduled {retry}/{max} {error}"))
            })),
            on_retry_attempt_start: None,
            on_retry_finished: Some(Arc::new(move |success, retry, _| {
                finished.lock().unwrap().push(format!("finished {success} {retry}"))
            })),
        };
        let policy = RetryPolicy { base_delay_ms: 1, ..RetryPolicy::default() };
        let source = responses.clone();
        let produce = move || -> BoxFuture<'static, AssistantMessage> {
            let next = source.lock().unwrap().pop();
            Box::pin(async move {
                next.unwrap_or_else(|| {
                    let mut ok = failed("");
                    ok.stop_reason = StopReason::Stop;
                    ok
                })
            })
        };
        let result = retry_assistant_call(produce, Some(&policy), &CancellationToken::new(), &callbacks).await;
        assert_eq!(result.stop_reason, StopReason::Stop);
        let expected = ["scheduled 1/3 terminated", "scheduled 2/3 terminated", "finished true 2"];
        assert_eq!(*events.lock().unwrap(), expected);
    }
}
