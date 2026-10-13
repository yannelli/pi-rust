//! Retry helpers. Provider requests retry transient HTTP failures and honor `retry-after`
//! (port of upstream `utils/provider-retry.ts`); the agent-level policy restarts failed
//! assistant turns (port of `utils/retry.ts`).

use std::collections::BTreeMap;
use std::future::Future;
use std::hash::{BuildHasher, Hasher};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::types::{AssistantMessage, StopReason};

/// Longest server-requested delay honored when the caller sets no limit.
pub const DEFAULT_MAX_RETRY_DELAY_MS: u64 = 60_000;
pub const DEFAULT_MAX_AGENT_RETRY_DELAY_MS: u64 = 60_000;

/// One failed provider request.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProviderRequestError {
    /// HTTP status, or `None` for transport failures and timeouts.
    pub status: Option<u16>,
    /// Response headers with lowercase names.
    pub headers: BTreeMap<String, String>,
    pub message: String,
    /// Bounded response body text, when one was read.
    pub body: Option<String>,
    /// The caller cancelled the request.
    pub aborted: bool,
}

impl ProviderRequestError {
    pub fn transport(message: impl Into<String>) -> Self {
        Self { message: message.into(), ..Self::default() }
    }

    pub fn aborted() -> Self {
        Self { message: "Request was aborted".into(), aborted: true, ..Self::default() }
    }
}

impl std::fmt::Display for ProviderRequestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ProviderRetryOptions<'a> {
    pub max_retries: u32,
    /// `None` uses 60 s; `Some(0)` disables the cap.
    pub max_retry_delay_ms: Option<u64>,
    pub cancel: Option<&'a CancellationToken>,
    /// Statuses that fail at once although the default policy would retry them.
    pub no_retry_statuses: &'a [u16],
}

/// Mirrors the OpenAI and Anthropic SDK policy: `x-should-retry`, then 408, 409, 429, 5xx, and transport errors.
pub fn is_retryable_provider_error(error: &ProviderRequestError) -> bool {
    match error.headers.get("x-should-retry").map(String::as_str) {
        Some("true") => return true,
        Some("false") => return false,
        _ => {}
    }
    match error.status {
        None => !error.aborted,
        Some(status) => matches!(status, 408 | 409 | 429) || status >= 500,
    }
}

fn validate_server_delay(delay_ms: f64, max_retry_delay_ms: Option<u64>, message: &str) -> Result<f64, String> {
    let max = max_retry_delay_ms.unwrap_or(DEFAULT_MAX_RETRY_DELAY_MS);
    if max > 0 && delay_ms > max as f64 {
        return Err(format!(
            "Server requested {}s retry delay (max: {}s). {message}",
            (delay_ms / 1000.0).ceil(),
            (max as f64 / 1000.0).ceil()
        ));
    }
    Ok(delay_ms)
}

/// Delay before retry `retry_index` (0-based): `retry-after-ms`, `retry-after` (seconds or an
/// HTTP date), else exponential backoff with jitter. Errors when the server asks for too long.
pub fn provider_retry_delay_ms(
    error: &ProviderRequestError,
    retry_index: u32,
    max_retry_delay_ms: Option<u64>,
) -> Result<u64, String> {
    if let Some(value) = error.headers.get("retry-after-ms").and_then(|value| value.trim().parse::<f64>().ok())
        && value.is_finite()
    {
        return validate_server_delay(value, max_retry_delay_ms, &error.message).map(|delay| delay.max(0.0) as u64);
    }
    if let Some(value) = error.headers.get("retry-after") {
        let delay = match value.trim().parse::<f64>() {
            Ok(seconds) => Some(seconds * 1000.0),
            Err(_) => parse_http_date_ms(value).map(|at| (at - crate::types::now_ms()) as f64),
        };
        if let Some(delay) = delay.filter(|delay| delay.is_finite()) {
            return validate_server_delay(delay, max_retry_delay_ms, &error.message).map(|delay| delay.max(0.0) as u64);
        }
    }
    let exponential = (0.5 * 2f64.powi(retry_index as i32)).min(8.0) * 1000.0;
    Ok((exponential * (1.0 - random_unit() * 0.25)) as u64)
}

fn random_unit() -> f64 {
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher
        .write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
    (hasher.finish() >> 11) as f64 / (1u64 << 53) as f64
}

/// Milliseconds since the epoch for an IMF-fixdate such as `Sun, 06 Nov 1994 08:49:37 GMT`.
pub fn parse_http_date_ms(value: &str) -> Option<i64> {
    let parts: Vec<&str> = value.split_whitespace().collect();
    let [_, day, month, year, time, "GMT"] = parts.as_slice() else { return None };
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let month = MONTHS.iter().position(|name| name == month)? as i64 + 1;
    let (day, year): (i64, i64) = (day.parse().ok()?, year.parse().ok()?);
    let clock: Vec<i64> = time.split(':').map(|part| part.parse().ok()).collect::<Option<_>>()?;
    let [hour, minute, second] = clock.as_slice() else { return None };
    // Days from a civil date (Howard Hinnant's algorithm).
    let shifted = if month <= 2 { year - 1 } else { year };
    let era = shifted.div_euclid(400);
    let year_of_era = shifted - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some(((days * 86_400) + hour * 3600 + minute * 60 + second) * 1000)
}

/// Sleep for `delay`, returning `false` when `cancel` fires first.
pub async fn cancellable_sleep(delay: Duration, cancel: Option<&CancellationToken>) -> bool {
    match cancel {
        Some(cancel) => tokio::select! {
            _ = cancel.cancelled() => false,
            _ = tokio::time::sleep(delay) => true,
        },
        None => {
            tokio::time::sleep(delay).await;
            true
        }
    }
}

/// Run `request` and retry retryable failures with interruptible backoff.
pub async fn retry_provider_request<T, F, Fut>(
    mut request: F,
    options: ProviderRetryOptions<'_>,
) -> Result<T, ProviderRequestError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ProviderRequestError>>,
{
    let mut remaining = options.max_retries;
    loop {
        let error = match request().await {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        if options.cancel.is_some_and(CancellationToken::is_cancelled) {
            return Err(ProviderRequestError::aborted());
        }
        let blocked = error.status.is_some_and(|status| options.no_retry_statuses.contains(&status));
        if remaining == 0 || !is_retryable_provider_error(&error) || blocked {
            return Err(error);
        }
        let retry_index = options.max_retries - remaining;
        remaining -= 1;
        let delay = provider_retry_delay_ms(&error, retry_index, options.max_retry_delay_ms)
            .map_err(|message| ProviderRequestError { message, ..error.clone() })?;
        if !cancellable_sleep(Duration::from_millis(delay), options.cancel).await {
            return Err(ProviderRequestError::aborted());
        }
    }
}

/// Agent-level retry policy, matching the `retry` settings of the coding agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    pub enabled: bool,
    /// Retries after the first call; 0 disables retrying.
    pub max_retries: u32,
    /// Delay of the first retry; each later retry doubles it.
    pub base_delay_ms: u64,
    /// Cap for each delay; `None` means 60 s.
    pub max_agent_delay_ms: Option<u64>,
}

/// Delay of retry number `retry` (1-based): the base delay doubled per retry, capped (default 60 s).
pub fn retry_delay_ms(base_delay_ms: u64, max_agent_delay_ms: Option<u64>, retry: u32) -> u64 {
    let factor = 2u64.checked_pow(retry.saturating_sub(1)).unwrap_or(u64::MAX);
    base_delay_ms.saturating_mul(factor).min(max_agent_delay_ms.unwrap_or(DEFAULT_MAX_AGENT_RETRY_DELAY_MS))
}

const NON_RETRYABLE_PATTERNS: &[&str] = &[
    "gousagelimiterror",
    "freeusagelimiterror",
    "monthly usage limit reached",
    "available balance",
    "insufficient_quota",
    "out of budget",
    "quota exceeded",
    "billing",
    "subscription_sharing_usage_limit_exceeded",
];

const RETRYABLE_PATTERNS: &[&str] = &[
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
    "enotfound",
    "eai_again",
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
    "resourceexhausted",
    "subscription_sharing_usage_unavailable",
    "subscription_sharing_user_unavailable",
];

/// Whether `pattern` (literal characters, `.` for any character, `?` making the character
/// before it optional) occurs in `text`. Both are lowercase.
fn pattern_found(text: &[char], pattern: &[char]) -> bool {
    fn matches_at(text: &[char], pattern: &[char]) -> bool {
        let Some((&atom, rest)) = pattern.split_first() else { return true };
        let optional = rest.first() == Some(&'?');
        let rest = if optional { &rest[1..] } else { rest };
        let here = text.first().is_some_and(|&c| atom == '.' || c == atom);
        (here && matches_at(&text[1..], rest)) || (optional && matches_at(text, rest))
    }
    (0..=text.len()).any(|start| matches_at(&text[start..], pattern))
}

fn matches_any(text: &str, patterns: &[&str]) -> bool {
    let text: Vec<char> = text.to_lowercase().chars().collect();
    patterns.iter().any(|pattern| pattern_found(&text, &pattern.chars().collect::<Vec<_>>()))
}

/// Whether a failed assistant message looks like a transient provider or transport error.
/// Quota and billing exhaustion never count. Callers handle context overflow first.
pub fn is_retryable_assistant_error(message: &AssistantMessage) -> bool {
    let Some(error) = message.error_message.as_deref().filter(|_| message.stop_reason == StopReason::Error) else {
        return false;
    };
    !error.is_empty() && !matches_any(error, NON_RETRYABLE_PATTERNS) && matches_any(error, RETRYABLE_PATTERNS)
}

/// Progress of [`retry_assistant_call`]. Retry numbers are 1-based.
#[derive(Clone, Debug, PartialEq)]
pub enum RetryEvent {
    /// Before the backoff sleep of a retry.
    Scheduled { retry: u32, max_retries: u32, delay_ms: u64, error_message: String },
    /// After the backoff sleep, before the retried call starts.
    RetryStart,
    /// Once, when a loop that retried at least once ends.
    Finished { success: bool, retry: u32, final_error: Option<String> },
}

/// Run `produce` and retry transient errors per `policy`. Aborts are never retried; an abort
/// during backoff returns the last error message with stop reason `Aborted`.
pub async fn retry_assistant_call<F, Fut>(
    mut produce: F,
    policy: Option<&RetryPolicy>,
    cancel: &CancellationToken,
    on_event: &mut (dyn FnMut(RetryEvent) + Send),
) -> AssistantMessage
where
    F: FnMut() -> Fut,
    Fut: Future<Output = AssistantMessage>,
{
    let max_retries = policy.filter(|policy| policy.enabled).map_or(0, |policy| policy.max_retries);
    let mut retry = 0;
    let mut retried = None;
    loop {
        let response = produce().await;
        match response.stop_reason {
            StopReason::Aborted => {
                if let Some(retry) = retried {
                    on_event(RetryEvent::Finished { success: false, retry, final_error: None });
                }
                return response;
            }
            StopReason::Error => {}
            _ => {
                if let Some(retry) = retried {
                    on_event(RetryEvent::Finished { success: true, retry, final_error: None });
                }
                return response;
            }
        }
        let Some(policy) = policy.filter(|_| retry < max_retries && is_retryable_assistant_error(&response)) else {
            if let Some(retry) = retried {
                let final_error = response.error_message.clone();
                on_event(RetryEvent::Finished { success: false, retry, final_error });
            }
            return response;
        };
        retry += 1;
        retried = Some(retry);
        let error_message = response.error_message.clone().unwrap_or_else(|| "Unknown error".into());
        let delay_ms = retry_delay_ms(policy.base_delay_ms, policy.max_agent_delay_ms, retry);
        on_event(RetryEvent::Scheduled { retry, max_retries, delay_ms, error_message: error_message.clone() });
        if !cancellable_sleep(Duration::from_millis(delay_ms), Some(cancel)).await {
            on_event(RetryEvent::Finished { success: false, retry, final_error: Some(error_message) });
            return AssistantMessage { stop_reason: StopReason::Aborted, error_message: None, ..response };
        }
        on_event(RetryEvent::RetryStart);
    }
}

#[cfg(test)]
#[path = "retry_tests.rs"]
mod tests;
