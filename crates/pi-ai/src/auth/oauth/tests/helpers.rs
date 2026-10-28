use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use tokio::sync::oneshot;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::support::{TestInteraction, answer, expect_err, get, pending};
use crate::auth::oauth::http::{decode_jwt_payload, parse_authorization_input};
use crate::auth::oauth::pkce::{base64_decode_lenient, random_hex};
use crate::auth::oauth::*;
use crate::auth::{AuthError, AuthPrompt};

#[test]
fn pkce_matches_the_rfc_7636_vector_and_generates_valid_pairs() {
    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    assert_eq!(pkce_challenge(verifier), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    let pkce = generate_pkce();
    assert_eq!(pkce.verifier.len(), 43);
    assert!(pkce.verifier.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    assert_eq!(pkce.challenge, pkce_challenge(&pkce.verifier));
    assert_ne!(generate_pkce().verifier, pkce.verifier);
    assert_eq!(base64url_encode(&[0xfb, 0xff]), "-_8");
}

#[test]
fn random_values_have_upstream_shapes() {
    let uuid = random_uuid();
    assert_eq!(uuid.len(), 36);
    assert_eq!(&uuid[14..15], "4");
    assert!(matches!(&uuid[19..20], "8" | "9" | "a" | "b"));
    assert_eq!(random_hex::<16>().len(), 32);
    assert_eq!(base64_decode_lenient("aGk"), Some(b"hi".to_vec()));
    assert_eq!(base64_decode_lenient("-_8"), Some(vec![0xfb, 0xff]));
}

#[test]
fn authorization_input_accepts_urls_fragments_queries_and_codes() {
    let parsed = parse_authorization_input(" http://localhost:1455/cb?code=c1&state=s1 ");
    assert_eq!((parsed.code.as_deref(), parsed.state.as_deref()), (Some("c1"), Some("s1")));
    let parsed = parse_authorization_input("c2#s2#extra");
    assert_eq!((parsed.code.as_deref(), parsed.state.as_deref()), (Some("c2"), Some("s2")));
    let parsed = parse_authorization_input("code=c3&state=s3");
    assert_eq!((parsed.code.as_deref(), parsed.state.as_deref()), (Some("c3"), Some("s3")));
    assert_eq!(parse_authorization_input("bare").code.as_deref(), Some("bare"));
    assert_eq!(parse_authorization_input("  ").code, None);
}

#[test]
fn jwt_payloads_decode_from_either_base64_alphabet() {
    let payload = base64url_encode(br#"{"sub":"a?>"}"#);
    assert_eq!(decode_jwt_payload(&format!("h.{payload}.s")).unwrap()["sub"], "a?>");
    assert!(decode_jwt_payload("only.two").is_none());
}

fn options(complete: CompleteFn<String>) -> CallbackServerOptions<String> {
    CallbackServerOptions {
        provider_name: "Example".into(),
        host: "127.0.0.1".into(),
        port: 0,
        path: "/callback".into(),
        redirect_host: None,
        state: Some("expected-state".into()),
        complete,
        cancel: None,
        timeout: None,
    }
}

fn completed() -> CompleteFn<String> {
    Arc::new(|code| async move { Ok(format!("completed:{code}")) }.boxed())
}

fn url(server: &CallbackServer<String>, query: &str) -> String {
    format!("{}?{query}", server.redirect_uri())
}

#[tokio::test]
async fn callback_server_ignores_stray_requests_and_completes_the_code() {
    let server = start_callback_server(options(completed())).await.unwrap();
    assert!(server.redirect_uri().starts_with("http://127.0.0.1:") && server.redirect_uri().ends_with("/callback"));
    let other = server.redirect_uri().replace("/callback", "/other");
    assert_eq!(get(&other).await.unwrap().0, 404);
    let (status, body) = get(&url(&server, "code=c&state=other")).await.unwrap();
    assert_eq!(status, 400);
    assert!(body.contains("State mismatch."));
    let post = reqwest::Client::new().post(url(&server, "code=c&state=expected-state")).send().await.unwrap();
    assert_eq!(post.status().as_u16(), 404);
    assert_eq!(get(&url(&server, "state=expected-state")).await.unwrap().0, 400);

    let response = reqwest::get(url(&server, "code=the-code&state=expected-state")).await.unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.headers()["content-type"], "text/html; charset=utf-8");
    let body = response.text().await.unwrap();
    assert!(body.contains("Authentication successful") && body.contains("Signed in to Example."));
    assert!(body.contains(r##"fill="#A33B1A""##));
    assert_eq!(server.wait().await.unwrap().as_deref(), Some("completed:the-code"));
}

#[tokio::test]
async fn callback_server_uses_the_redirect_host_and_skips_absent_state() {
    let mut options = options(completed());
    options.redirect_host = Some("localhost".into());
    options.state = None;
    let server = start_callback_server(options).await.unwrap();
    assert!(server.redirect_uri().starts_with("http://localhost:"));
    let local = server.redirect_uri().replace("localhost", "127.0.0.1");
    assert_eq!(get(&format!("{local}?code=no-state")).await.unwrap().0, 200);
    assert_eq!(server.wait().await.unwrap().as_deref(), Some("completed:no-state"));
}

#[tokio::test]
async fn callback_server_reports_completion_failures_and_error_redirects() {
    let failing: CompleteFn<String> = Arc::new(|_| async { Err(AuthError::other("token exchange failed")) }.boxed());
    let server = start_callback_server(options(failing)).await.unwrap();
    let (status, body) = get(&url(&server, "code=c&state=expected-state")).await.unwrap();
    assert_eq!(status, 502);
    assert!(body.contains("Example sign-in failed.") && body.contains("token exchange failed"));
    assert_eq!(expect_err(server.wait().await), "token exchange failed");

    let server = start_callback_server(options(completed())).await.unwrap();
    let query = "error=access_denied&error_description=User+denied+access&state=expected-state";
    let (status, body) = get(&url(&server, query)).await.unwrap();
    assert_eq!(status, 400);
    assert!(body.contains("User denied access"));
    assert_eq!(expect_err(server.wait().await), "Example authorization failed: User denied access");
}

#[tokio::test]
async fn callback_server_completes_only_the_first_callback() {
    let (finish, finished) = oneshot::channel::<String>();
    let (started, exchange_started) = oneshot::channel::<()>();
    let channels = Arc::new(std::sync::Mutex::new(Some((finished, started))));
    let complete: CompleteFn<String> = Arc::new(move |_| {
        let (receiver, started) = channels.lock().unwrap().take().expect("one exchange");
        let _ = started.send(());
        async move { Ok(receiver.await.unwrap()) }.boxed()
    });
    let server = start_callback_server(options(complete)).await.unwrap();
    let target = url(&server, "code=c&state=expected-state");
    let first = tokio::spawn({
        let target = target.clone();
        async move { get(&target).await.unwrap().0 }
    });
    exchange_started.await.unwrap();
    assert_eq!(get(&target).await.unwrap().0, 409);
    // A claimed callback keeps completing even when the caller switches to manual input.
    server.cancel();
    finish.send("done".into()).unwrap();
    assert_eq!(first.await.unwrap(), 200);
    assert_eq!(server.wait().await.unwrap().as_deref(), Some("done"));
}

#[tokio::test]
async fn callback_server_cancel_resolves_empty_and_later_callbacks_conflict() {
    let server = start_callback_server(options(completed())).await.unwrap();
    server.cancel();
    assert_eq!(server.wait().await.unwrap(), None);
    assert_eq!(get(&url(&server, "code=c&state=expected-state")).await.unwrap().0, 409);
}

#[tokio::test]
async fn callback_server_fails_on_cancel_timeout_and_taken_ports() {
    let cancel = CancellationToken::new();
    let mut aborted = options(completed());
    aborted.cancel = Some(cancel.clone());
    let server = start_callback_server(aborted).await.unwrap();
    cancel.cancel();
    assert!(matches!(server.wait().await, Err(AuthError::Cancelled)));

    let mut timed = options(completed());
    timed.timeout = Some(Duration::from_millis(10));
    let server = start_callback_server(timed).await.unwrap();
    assert_eq!(expect_err(server.wait().await), "Example sign-in timed out");

    let mut already = options(completed());
    already.cancel = Some(cancel);
    assert!(matches!(start_callback_server(already).await, Err(AuthError::Cancelled)));

    let blocker = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut taken = options(completed());
    taken.port = blocker.local_addr().unwrap().port();
    assert!(start_callback_server(taken).await.is_err());
}

#[tokio::test]
async fn close_stops_listening() {
    let server = start_callback_server(options(completed())).await.unwrap();
    let target = url(&server, "code=c&state=expected-state");
    server.close();
    assert_eq!(expect_err(server.wait().await), "OAuth callback server closed");
    tokio::task::yield_now().await;
    assert!(get(&target).await.is_err());
}

#[tokio::test]
async fn race_returns_the_browser_callback_and_dismisses_the_prompt() {
    let mut options = options(Arc::new(|code| async move { Ok(code) }.boxed()));
    options.state = None;
    let server = start_callback_server(options).await.unwrap();
    let interaction = TestInteraction::new(|_, cancel, _| pending(cancel));
    let target = format!("{}?code=from-browser", server.redirect_uri());
    tokio::spawn(async move { get(&target).await });
    let cancel = CancellationToken::new();
    let result = wait_for_callback_or_manual_input(&interaction, Some(&server), "paste", "p", &cancel).await;
    assert_eq!(result.unwrap(), CallbackOrManual::Callback("from-browser".into()));
    let prompts = interaction.prompts.lock().unwrap();
    assert!(matches!(&prompts[0].0, AuthPrompt::ManualCode { message, .. } if message == "paste"));
    assert!(prompts[0].1.is_cancelled());
}

#[tokio::test]
async fn race_prefers_pasted_input_and_propagates_prompt_failures() {
    let server = start_callback_server(options(completed())).await.unwrap();
    let interaction = TestInteraction::new(|_, _, _| answer("pasted"));
    let cancel = CancellationToken::new();
    let result = wait_for_callback_or_manual_input(&interaction, Some(&server), "paste", "p", &cancel).await;
    assert_eq!(result.unwrap(), CallbackOrManual::Manual("pasted".into()));
    // The browser lost the race, so its callback is refused.
    assert_eq!(get(&url(&server, "code=c&state=expected-state")).await.unwrap().0, 409);

    let result = wait_for_callback_or_manual_input::<String>(&interaction, None, "paste", "p", &cancel).await;
    assert_eq!(result.unwrap(), CallbackOrManual::Manual("pasted".into()));

    let server = start_callback_server(options(completed())).await.unwrap();
    let failing = TestInteraction::new(|_, _, _| async { Err(AuthError::other("prompt cancelled")) }.boxed());
    let result = wait_for_callback_or_manual_input(&failing, Some(&server), "paste", "p", &cancel).await;
    assert_eq!(expect_err(result), "prompt cancelled");
}

fn poll_options(interval: f64, expires: f64, wait_first: bool) -> DeviceCodePollOptions {
    DeviceCodePollOptions {
        interval_seconds: Some(interval),
        expires_in_seconds: Some(expires),
        wait_before_first_poll: wait_first,
    }
}

async fn poll_script(
    options: DeviceCodePollOptions,
    script: Vec<DevicePoll<&'static str>>,
) -> (Result<&'static str, String>, Vec<u128>) {
    let start = Instant::now();
    let script = std::sync::Mutex::new(std::collections::VecDeque::from(script));
    let times = std::sync::Mutex::new(Vec::new());
    let cancel = CancellationToken::new();
    let result = poll_device_code_flow(options, &cancel, || {
        times.lock().unwrap().push(start.elapsed().as_millis());
        let next = script.lock().unwrap().pop_front().expect("unexpected extra poll");
        async move { Ok(next) }
    })
    .await;
    (result.map_err(|error| error.to_string()), times.into_inner().unwrap())
}

#[tokio::test(start_paused = true)]
async fn device_polling_follows_the_interval_and_slow_down_rules() {
    let script = vec![DevicePoll::Pending, DevicePoll::Complete("token")];
    assert_eq!(poll_script(poll_options(2.0, 30.0, false), script).await, (Ok("token"), vec![0, 2000]));

    let script = vec![DevicePoll::Complete("token")];
    assert_eq!(poll_script(poll_options(2.0, 30.0, true), script).await, (Ok("token"), vec![2000]));

    let script = vec![DevicePoll::SlowDown { interval_seconds: None }, DevicePoll::Complete("token")];
    assert_eq!(poll_script(poll_options(2.0, 900.0, false), script).await, (Ok("token"), vec![0, 7000]));

    let script = vec![DevicePoll::SlowDown { interval_seconds: Some(30.0) }, DevicePoll::Complete("token")];
    assert_eq!(poll_script(poll_options(2.0, 900.0, false), script).await, (Ok("token"), vec![0, 30000]));

    let script = vec![DevicePoll::Pending, DevicePoll::Failed("denied".into())];
    assert_eq!(poll_script(poll_options(0.2, 30.0, false), script).await, (Err("denied".into()), vec![0, 1000]));
}

#[tokio::test(start_paused = true)]
async fn device_polling_times_out_and_cancels() {
    let script = vec![DevicePoll::Pending, DevicePoll::Pending];
    let (result, times) = poll_script(poll_options(5.0, 8.0, false), script).await;
    assert_eq!((result, times), (Err("Device flow timed out".into()), vec![0, 5000]));

    let script = vec![DevicePoll::SlowDown { interval_seconds: None }];
    let (result, _) = poll_script(poll_options(5.0, 8.0, false), script).await;
    assert!(result.unwrap_err().starts_with("Device flow timed out after one or more slow_down responses"));

    let cancel = CancellationToken::new();
    let polling =
        poll_device_code_flow(poll_options(5.0, 30.0, false), &cancel, || async { Ok(DevicePoll::<()>::Pending) });
    cancel.cancel();
    assert!(matches!(polling.await, Err(AuthError::Cancelled)));
}
