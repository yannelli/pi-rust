//! RFC 8628 device-code polling shared by the device login flows.

use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::auth::{AuthError, AuthResult};

const TIMEOUT_MESSAGE: &str = "Device flow timed out";
const SLOW_DOWN_TIMEOUT_MESSAGE: &str = "Device flow timed out after one or more slow_down responses. This is often caused by clock drift in WSL or VM environments. Please sync or restart the VM clock and try again.";
const MINIMUM_INTERVAL_MS: u64 = 1000;
// RFC 8628 section 3.2: without `interval` the client must wait 5 seconds.
const DEFAULT_POLL_INTERVAL_SECONDS: f64 = 5.0;
// RFC 8628 section 3.5: `slow_down` adds 5 seconds to the interval.
const SLOW_DOWN_INTERVAL_INCREMENT_MS: u64 = 5000;

/// The outcome of one poll of the token endpoint.
#[derive(Debug, PartialEq)]
pub enum DevicePoll<T> {
    Pending,
    /// Slow down, to the server's interval when it sends one.
    SlowDown {
        interval_seconds: Option<f64>,
    },
    Failed(String),
    Complete(T),
}

/// Polling schedule from the device authorization response.
#[derive(Clone, Copy, Debug, Default)]
pub struct DeviceCodePollOptions {
    pub interval_seconds: Option<f64>,
    pub expires_in_seconds: Option<f64>,
    pub wait_before_first_poll: bool,
}

fn interval_ms(seconds: f64) -> u64 {
    MINIMUM_INTERVAL_MS.max((seconds * 1000.0).floor() as u64)
}

/// Polls until `poll` completes or fails, the device code expires, or `cancel` fires.
/// Errors from `poll` end polling and propagate unchanged.
pub async fn poll_device_code_flow<T, F, Fut>(
    options: DeviceCodePollOptions,
    cancel: &CancellationToken,
    mut poll: F,
) -> AuthResult<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = AuthResult<DevicePoll<T>>>,
{
    let deadline = options
        .expires_in_seconds
        .filter(|seconds| seconds.is_finite())
        .map(|seconds| Instant::now() + Duration::from_millis((seconds.max(0.0) * 1000.0) as u64));
    let remaining = || deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
    let mut interval = interval_ms(options.interval_seconds.unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS));
    let mut slow_down_responses = 0u32;

    if options.wait_before_first_poll {
        let wait = Duration::from_millis(interval);
        match remaining() {
            Some(left) if left.is_zero() => {}
            Some(left) => cancellable_sleep(wait.min(left), cancel).await?,
            None => cancellable_sleep(wait, cancel).await?,
        }
    }

    while remaining().is_none_or(|left| !left.is_zero()) {
        if cancel.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        match poll().await? {
            DevicePoll::Complete(value) => return Ok(value),
            DevicePoll::Failed(message) => return Err(AuthError::Other(message)),
            DevicePoll::Pending => {}
            DevicePoll::SlowDown { interval_seconds } => {
                slow_down_responses += 1;
                // Trust a server interval (GitHub sends the new minimum); otherwise add 5 seconds.
                interval = match interval_seconds.filter(|seconds| seconds.is_finite() && *seconds > 0.0) {
                    Some(seconds) => interval_ms(seconds),
                    None => MINIMUM_INTERVAL_MS.max(interval + SLOW_DOWN_INTERVAL_INCREMENT_MS),
                };
            }
        }
        let wait = Duration::from_millis(interval);
        match remaining() {
            Some(left) if left.is_zero() => break,
            Some(left) => cancellable_sleep(wait.min(left), cancel).await?,
            None => cancellable_sleep(wait, cancel).await?,
        }
    }

    let message = if slow_down_responses > 0 { SLOW_DOWN_TIMEOUT_MESSAGE } else { TIMEOUT_MESSAGE };
    Err(AuthError::Other(message.into()))
}

/// Sleeps, failing with [`AuthError::Cancelled`] when `cancel` fires first.
pub async fn cancellable_sleep(duration: Duration, cancel: &CancellationToken) -> AuthResult<()> {
    super::http::sleep(duration, cancel).await
}
