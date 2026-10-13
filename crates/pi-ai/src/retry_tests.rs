use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;

fn provider_error(status: u16, headers: &[(&str, &str)]) -> ProviderRequestError {
    ProviderRequestError {
        status: Some(status),
        headers: headers.iter().map(|(name, value)| (name.to_string(), value.to_string())).collect(),
        message: format!("Provider error: {status}"),
        ..Default::default()
    }
}

/// A request that fails with `errors` in order, then succeeds.
fn scripted(
    errors: Vec<ProviderRequestError>,
) -> (Arc<AtomicU32>, impl FnMut() -> std::future::Ready<Result<&'static str, ProviderRequestError>>) {
    let calls = Arc::new(AtomicU32::new(0));
    let counter = calls.clone();
    let request = move || {
        let call = counter.fetch_add(1, Ordering::SeqCst) as usize;
        std::future::ready(errors.get(call).cloned().map_or(Ok("ok"), Err))
    };
    (calls, request)
}

#[tokio::test(start_paused = true)]
async fn retries_retryable_errors_after_the_server_delay() {
    let (calls, request) = scripted(vec![provider_error(429, &[("retry-after-ms", "1000")])]);
    let started = tokio::time::Instant::now();
    let result = retry_provider_request(request, ProviderRetryOptions { max_retries: 1, ..Default::default() }).await;
    assert_eq!(result, Ok("ok"));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(started.elapsed(), Duration::from_millis(1000));
}

#[tokio::test]
async fn non_retryable_and_blocked_statuses_fail_at_once() {
    let (calls, request) = scripted(vec![provider_error(429, &[("x-should-retry", "false")]); 3]);
    let error = retry_provider_request(request, ProviderRetryOptions { max_retries: 2, ..Default::default() }).await;
    assert_eq!(error.unwrap_err().status, Some(429));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let (calls, request) = scripted(vec![provider_error(504, &[("retry-after-ms", "0")]); 3]);
    let options = ProviderRetryOptions { max_retries: 2, no_retry_statuses: &[504], ..Default::default() };
    assert!(retry_provider_request(request, options).await.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let (calls, request) = scripted(vec![provider_error(400, &[]); 3]);
    assert!(
        retry_provider_request(request, ProviderRetryOptions { max_retries: 2, ..Default::default() }).await.is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn long_server_delays_fail_with_the_requested_delay() {
    let (calls, request) = scripted(vec![provider_error(429, &[("retry-after", "277403")])]);
    let options = ProviderRetryOptions { max_retries: 1, max_retry_delay_ms: Some(1000), ..Default::default() };
    let error = retry_provider_request(request, options).await.unwrap_err();
    assert!(error.message.starts_with("Server requested 277403s retry delay (max: 1s)"), "{}", error.message);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn a_zero_cap_honors_any_delay_and_cancellation_interrupts_it() {
    let (calls, request) = scripted(vec![provider_error(429, &[("retry-after", "2")])]);
    let options = ProviderRetryOptions { max_retries: 1, max_retry_delay_ms: Some(0), ..Default::default() };
    let started = tokio::time::Instant::now();
    assert_eq!(retry_provider_request(request, options).await, Ok("ok"));
    assert_eq!((calls.load(Ordering::SeqCst), started.elapsed()), (2, Duration::from_secs(2)));

    let cancel = CancellationToken::new();
    let (calls, request) = scripted(vec![provider_error(429, &[("retry-after", "277403")]); 3]);
    let options = ProviderRetryOptions {
        max_retries: 2,
        max_retry_delay_ms: Some(0),
        cancel: Some(&cancel),
        ..Default::default()
    };
    let canceller = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        canceller.cancel();
    });
    let error = retry_provider_request(request, options).await.unwrap_err();
    assert!(error.aborted);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn exponential_backoff_and_http_dates() {
    for retry_index in 0..6 {
        let delay = provider_retry_delay_ms(&provider_error(503, &[]), retry_index, None).unwrap();
        let max = (500u64 << retry_index).min(8000);
        assert!(delay <= max && delay >= max * 3 / 4, "{retry_index}: {delay}");
    }
    assert_eq!(parse_http_date_ms("Sun, 06 Nov 1994 08:49:37 GMT"), Some(784_111_777_000));
    assert_eq!(parse_http_date_ms("tomorrow"), None);
}

fn error_message(text: &str) -> AssistantMessage {
    AssistantMessage { stop_reason: StopReason::Error, error_message: Some(text.into()), ..Default::default() }
}

#[test]
fn classifies_retryable_assistant_errors() {
    for text in [
        "An error occurred while processing your request. You can retry your request, or contact us.",
        r#"{"message":"The system encountered an unexpected error during processing. Try your request again."}"#,
        "ResourceExhausted: Worker local total request limit reached (288/48)",
        "The socket connection was closed unexpectedly.",
        "Error: exceeded request buffer limit while retrying upstream",
        "The pending stream has been canceled (caused by: getaddrinfo ENOTFOUND bedrock.amazonaws.com)",
        "connect ENOTFOUND api.example.com",
        "EAI_AGAIN api.example.com",
        "OpenAI Responses stream ended before a terminal response event",
        "The system is currently experiencing high demand and cannot process your request.",
        "subscription_sharing_usage_unavailable: Usage cannot be checked.",
        "overloaded_error",
        "520 status code (no body)",
        "Rate-limit hit",
        "request timed out",
        "Request time out",
    ] {
        assert!(is_retryable_assistant_error(&error_message(text)), "{text}");
    }
    for text in [
        "429 quota exceeded",
        r#"OpenAI API error (429): {"code":"subscription_sharing_usage_limit_exceeded"}"#,
        "invalid api key",
    ] {
        assert!(!is_retryable_assistant_error(&error_message(text)), "{text}");
    }
    assert!(!is_retryable_assistant_error(&AssistantMessage::default()));
}

#[test]
fn agent_retry_delay_is_capped() {
    assert_eq!(retry_delay_ms(2000, None, 6), 60_000);
    assert_eq!(retry_delay_ms(2000, Some(5000), 5), 5000);
    assert_eq!(retry_delay_ms(2000, Some(0), 5), 0);
    assert_eq!(retry_delay_ms(1000, None, 2), 2000);
}

#[tokio::test(start_paused = true)]
async fn retry_assistant_call_follows_the_policy() {
    let policy = RetryPolicy { enabled: true, max_retries: 3, base_delay_ms: 10, max_agent_delay_ms: None };
    let responses = |items: Vec<AssistantMessage>| {
        Arc::new(std::sync::Mutex::new(items.into_iter().collect::<std::collections::VecDeque<_>>()))
    };
    let ok = AssistantMessage { stop_reason: StopReason::Stop, ..Default::default() };
    let queue = responses(vec![error_message("overloaded"), error_message("503"), ok.clone()]);
    let mut events = Vec::new();
    let produce = || {
        let next = queue.lock().unwrap().pop_front().unwrap();
        std::future::ready(next)
    };
    let result =
        retry_assistant_call(produce, Some(&policy), &CancellationToken::new(), &mut |event| events.push(event)).await;
    assert_eq!(result.stop_reason, StopReason::Stop);
    assert_eq!(
        events,
        [
            RetryEvent::Scheduled { retry: 1, max_retries: 3, delay_ms: 10, error_message: "overloaded".into() },
            RetryEvent::RetryStart,
            RetryEvent::Scheduled { retry: 2, max_retries: 3, delay_ms: 20, error_message: "503".into() },
            RetryEvent::RetryStart,
            RetryEvent::Finished { success: true, retry: 2, final_error: None },
        ]
    );

    let queue = responses(vec![error_message("insufficient_quota")]);
    let mut events = Vec::new();
    let produce = || std::future::ready(queue.lock().unwrap().pop_front().unwrap());
    let result =
        retry_assistant_call(produce, Some(&policy), &CancellationToken::new(), &mut |event| events.push(event)).await;
    assert_eq!((result.stop_reason, events.len()), (StopReason::Error, 0));

    let disabled = RetryPolicy { enabled: false, ..policy };
    let queue = responses(vec![error_message("overloaded")]);
    let produce = || std::future::ready(queue.lock().unwrap().pop_front().unwrap());
    let result = retry_assistant_call(produce, Some(&disabled), &CancellationToken::new(), &mut |_| {}).await;
    assert_eq!(result.stop_reason, StopReason::Error);

    let cancel = CancellationToken::new();
    cancel.cancel();
    let queue = responses(vec![error_message("overloaded")]);
    let mut events = Vec::new();
    let produce = || std::future::ready(queue.lock().unwrap().pop_front().unwrap());
    let result = retry_assistant_call(produce, Some(&policy), &cancel, &mut |event| events.push(event)).await;
    assert_eq!((result.stop_reason, result.error_message), (StopReason::Aborted, None));
    assert_eq!(
        events.last(),
        Some(&RetryEvent::Finished { success: false, retry: 1, final_error: Some("overloaded".into()) })
    );
}
