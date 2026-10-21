//! Retrying fetch for idempotent management requests such as version checks, catalogs, and
//! downloads (upstream `utils/management-http.ts`), plus the pi user agent.

use std::fmt;
use std::time::Duration;

use pi_ai::{ClientIdentity, UserAgentMode};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Version of this build.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const RETRYABLE_STATUS_CODES: [u16; 7] = [408, 425, 429, 500, 502, 503, 504];

/// pi's `User-Agent` for its own network requests; the same detailed form pi-ai sends to providers.
pub fn pi_user_agent(version: &str) -> String {
    let identity = ClientIdentity {
        user_agent: Some(UserAgentMode::Detailed),
        version: Some(version.into()),
        ..ClientIdentity::default()
    };
    pi_ai::format_user_agent(Some(&identity))
}

#[derive(Clone, Debug)]
pub struct FetchRetryOptions {
    /// Retries after the first request.
    pub max_retries: u32,
    /// Retry transient HTTP statuses as well as transport failures.
    pub retry_on_status: bool,
    /// Time limit for all requests together; reaching it ends the fetch.
    pub timeout: Option<Duration>,
    /// Time limit of one request; a request that hangs is retried.
    pub attempt_timeout: Option<Duration>,
}

impl Default for FetchRetryOptions {
    fn default() -> Self {
        Self { max_retries: 2, retry_on_status: true, timeout: None, attempt_timeout: None }
    }
}

#[derive(Debug)]
pub enum FetchError {
    Cancelled,
    TimedOut,
    Request(reqwest::Error),
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("Request cancelled"),
            Self::TimedOut => f.write_str("Request timed out"),
            Self::Request(error) => f.write_str(&format_network_error(error)),
        }
    }
}

impl std::error::Error for FetchError {}

fn errno_code(kind: std::io::ErrorKind) -> String {
    use std::io::ErrorKind::*;
    match kind {
        ConnectionRefused => "ECONNREFUSED".into(),
        ConnectionReset => "ECONNRESET".into(),
        ConnectionAborted => "ECONNABORTED".into(),
        TimedOut => "ETIMEDOUT".into(),
        HostUnreachable => "EHOSTUNREACH".into(),
        NetworkUnreachable => "ENETUNREACH".into(),
        AddrNotAvailable => "EADDRNOTAVAIL".into(),
        other => format!("{other:?}"),
    }
}

/// The error message plus the I/O error codes or first cause message found in its source chain.
pub fn format_network_error(error: &(dyn std::error::Error + 'static)) -> String {
    let root = error.to_string();
    let mut codes: Vec<String> = Vec::new();
    let mut cause = None;
    let mut source = error.source();
    while let Some(current) = source {
        if let Some(io) = current.downcast_ref::<std::io::Error>() {
            let code = errno_code(io.kind());
            if !codes.contains(&code) {
                codes.push(code);
            }
        } else if cause.is_none() && !current.to_string().is_empty() {
            cause = Some(current.to_string());
        }
        source = current.source();
    }
    if !codes.is_empty() {
        return format!("{root} ({})", codes.join(", "));
    }
    match cause {
        Some(cause) if cause != root => format!("{root} (cause: {cause})"),
        _ => root,
    }
}

/// Send the request built by `build`, retrying transport failures, request timeouts, and
/// (unless disabled) retryable statuses. Cancellation and the overall timeout are terminal.
pub async fn fetch_with_retry(
    build: impl Fn() -> reqwest::RequestBuilder,
    options: &FetchRetryOptions,
    cancel: &CancellationToken,
) -> Result<reqwest::Response, FetchError> {
    let deadline = options.timeout.filter(|timeout| !timeout.is_zero()).map(|timeout| Instant::now() + timeout);
    let attempt_timeout = options.attempt_timeout.filter(|timeout| !timeout.is_zero());
    let mut attempt = 0;
    loop {
        if cancel.is_cancelled() {
            return Err(FetchError::Cancelled);
        }
        let now = Instant::now();
        if deadline.is_some_and(|deadline| now >= deadline) {
            return Err(FetchError::TimedOut);
        }
        let limit = match (deadline.map(|deadline| deadline - now), attempt_timeout) {
            (Some(left), Some(each)) => Some(left.min(each)),
            (left, each) => left.or(each),
        };
        let send = build().send();
        let outcome = tokio::select! {
            _ = cancel.cancelled() => return Err(FetchError::Cancelled),
            result = async {
                match limit {
                    Some(limit) => tokio::time::timeout(limit, send).await.ok(),
                    None => Some(send.await),
                }
            } => result,
        };
        match outcome {
            Some(Ok(response)) => {
                let status = response.status().as_u16();
                let retry = options.retry_on_status && RETRYABLE_STATUS_CODES.contains(&status);
                if !retry || attempt >= options.max_retries {
                    return Ok(response);
                }
            }
            Some(Err(error)) if attempt >= options.max_retries => return Err(FetchError::Request(error)),
            Some(Err(_)) => {}
            None if deadline.is_some_and(|deadline| Instant::now() >= deadline) => return Err(FetchError::TimedOut),
            None if attempt >= options.max_retries => return Err(FetchError::TimedOut),
            None => {}
        }
        attempt += 1;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::platform::test_http::{TestReply, TestServer, client};

    #[test]
    fn user_agent_names_version_and_platform() {
        let agent = pi_user_agent("1.2.3");
        assert!(agent.starts_with("pi/1.2.3 ("), "{agent}");
        assert!(agent.contains("; rust; "), "{agent}");
    }

    #[tokio::test]
    async fn retries_transient_statuses_up_to_the_limit() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let server = TestServer::start(move |_| {
            if counter.fetch_add(1, Ordering::SeqCst) < 2 {
                TestReply::status(503, "busy")
            } else {
                TestReply::status(200, "ok")
            }
        })
        .await;
        let http = client();
        let url = server.url.clone();
        let cancel = CancellationToken::new();
        let response = fetch_with_retry(|| http.get(&url), &FetchRetryOptions::default(), &cancel).await.unwrap();
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        let no_retry = FetchRetryOptions { max_retries: 0, ..FetchRetryOptions::default() };
        calls.store(0, Ordering::SeqCst);
        let response = fetch_with_retry(|| http.get(&url), &no_retry, &cancel).await.unwrap();
        assert_eq!(response.status().as_u16(), 503);
    }

    #[tokio::test]
    async fn transport_failures_retry_then_report_the_cause() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let http = client();
        let options = FetchRetryOptions { max_retries: 1, ..FetchRetryOptions::default() };
        let error = fetch_with_retry(|| http.get(&url), &options, &CancellationToken::new()).await.unwrap_err();
        assert!(matches!(error, FetchError::Request(_)));
        assert!(!error.to_string().is_empty());

        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = fetch_with_retry(|| http.get(&url), &options, &cancel).await.unwrap_err();
        assert!(matches!(error, FetchError::Cancelled));
    }

    #[test]
    fn network_errors_list_io_codes() {
        #[derive(Debug)]
        struct Wrapper(std::io::Error);
        impl fmt::Display for Wrapper {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("fetch failed")
            }
        }
        impl std::error::Error for Wrapper {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
        let error = Wrapper(std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timeout"));
        assert_eq!(format_network_error(&error), "fetch failed (ETIMEDOUT)");
    }
}
