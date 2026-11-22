use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pi_ai::auth::{AuthEvent, AuthInteraction, AuthPrompt, AuthResult};
use serde_json::{Value, json};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::export::pi_rust_share::{
    ACCOUNT_FILE, DeviceAuthorization, MAX_SHARE_BYTES, PiRustAccount, PiRustError, delete_account,
    fetch_account_email, load_account, load_account_for, poll_device_token, revoke_token, save_account, sign_in,
    start_device_authorization, upload_share,
};
use crate::platform::test_http::{TestReply, TestRequest, TestServer, client};

const EXPIRED: &str = "The pi-rust.com sign-in code expired. Run /share again to get a new code.";

fn reply(status: u16, body: Value) -> TestReply {
    TestReply::status(status, &body.to_string()).header("content-type", "application/json")
}

fn form(request: &TestRequest) -> HashMap<String, String> {
    url::form_urlencoded::parse(&request.body).into_owned().collect()
}

fn device(interval: Option<u64>, expires_in: u64) -> Value {
    json!({
        "device_code": "dev-123", "user_code": "WXYZ-1234",
        "verification_uri": "https://share.test/device",
        "verification_uri_complete": "https://share.test/device?user_code=WXYZ-1234",
        "expires_in": expires_in, "interval": interval,
    })
}

/// A share service whose token endpoint answers `polls` in order, recording when each poll arrived.
struct Service {
    server: TestServer,
    poll_times: Arc<Mutex<Vec<Instant>>>,
}

async fn service(device: Value, polls: Vec<(u16, Value)>) -> Service {
    let polls = Arc::new(Mutex::new(VecDeque::from(polls)));
    let poll_times = Arc::new(Mutex::new(Vec::new()));
    let times = poll_times.clone();
    let server = TestServer::start(move |request| match request.path() {
        "/oauth/device_authorization" => reply(200, device.clone()),
        "/oauth/token" => {
            times.lock().unwrap().push(Instant::now());
            let (status, body) = polls.lock().unwrap().pop_front().expect("an unexpected poll");
            reply(status, body)
        }
        "/v1/account" => reply(200, json!({"ok": true, "account": {"email": "me@account.test"}})),
        other => panic!("unexpected request to {other}"),
    })
    .await;
    Service { server, poll_times }
}

fn token(email: Option<&str>) -> (u16, Value) {
    let mut body = json!({"access_token": "tok-1", "token_type": "Bearer", "scope": "share"});
    if let Some(email) = email {
        body["account"] = json!({"email": email});
    }
    (200, body)
}

fn oauth_error(code: &str) -> (u16, Value) {
    (400, json!({"error": code}))
}

#[derive(Default)]
struct Recorder(Mutex<Vec<AuthEvent>>);

#[async_trait]
impl AuthInteraction for Recorder {
    async fn prompt(&self, _: AuthPrompt, _: CancellationToken) -> AuthResult<String> {
        panic!("the device flow does not prompt")
    }

    fn notify(&self, event: AuthEvent) {
        self.0.lock().unwrap().push(event);
    }
}

#[tokio::test(start_paused = true)]
async fn sign_in_polls_through_pending_and_slow_down() {
    let polls = vec![oauth_error("authorization_pending"), oauth_error("slow_down"), token(Some("me@share.test"))];
    let service = service(device(Some(1), 600), polls).await;
    let interaction = Recorder::default();
    let start = Instant::now();
    let account = sign_in(&client(), &service.server.url, &interaction, &CancellationToken::new()).await.unwrap();
    assert_eq!(account.origin, service.server.url);
    assert_eq!((account.token.as_str(), account.email.as_deref()), ("tok-1", Some("me@share.test")));

    // Wait one interval, poll, wait again; slow_down adds 5 seconds to the interval.
    let times: Vec<u128> = service.poll_times.lock().unwrap().iter().map(|at| (*at - start).as_millis()).collect();
    assert_eq!(times, [1000, 2000, 8000]);
    let requests = service.server.requests();
    let authorize = &requests[0];
    assert_eq!((authorize.method.as_str(), authorize.path()), ("POST", "/oauth/device_authorization"));
    assert_eq!(authorize.header("content-type"), Some("application/x-www-form-urlencoded"));
    assert_eq!(
        form(authorize),
        HashMap::from([("client_id".into(), "pi-cli".into()), ("scope".into(), "share".into())])
    );
    let poll = form(&requests[1]);
    assert_eq!(poll["grant_type"], "urn:ietf:params:oauth:grant-type:device_code");
    assert_eq!((poll["device_code"].as_str(), poll["client_id"].as_str()), ("dev-123", "pi-cli"));
    assert!(requests[1].header("user-agent").is_some_and(|agent| agent.starts_with("pi/")));

    let events = interaction.0.lock().unwrap().clone();
    let expected = AuthEvent::DeviceCode {
        user_code: "WXYZ-1234".into(),
        verification_uri: "https://share.test/device?user_code=WXYZ-1234".into(),
        open_browser_on_confirm: None,
        copy_code_on_confirm: None,
        interval_seconds: Some(1),
        expires_in_seconds: Some(600),
    };
    assert_eq!(events, [expected]);
}

#[tokio::test(start_paused = true)]
async fn sign_in_reads_the_email_from_the_account_when_the_token_has_none() {
    let service = service(device(Some(1), 600), vec![token(None)]).await;
    let account = sign_in(&client(), &service.server.url, &Recorder::default(), &CancellationToken::new()).await;
    assert_eq!(account.unwrap().email.as_deref(), Some("me@account.test"));
    let account = service.server.requests().into_iter().find(|request| request.path() == "/v1/account").unwrap();
    assert_eq!(account.header("authorization"), Some("Bearer tok-1"));
}

#[tokio::test(start_paused = true)]
async fn denied_and_expired_sign_ins_fail() {
    let failures = [
        (oauth_error("access_denied"), "pi-rust.com sign-in was denied."),
        (oauth_error("expired_token"), EXPIRED),
        ((400, json!({"error": "invalid_grant"})), "pi-rust.com sign-in failed: invalid_grant"),
    ];
    for (poll, message) in failures {
        let service = service(device(Some(1), 600), vec![poll]).await;
        let result = sign_in(&client(), &service.server.url, &Recorder::default(), &CancellationToken::new()).await;
        assert_eq!(result, Err(PiRustError::Failed(message.into())));
    }

    // The code expires after `expires_in` seconds even when the server keeps answering pending.
    let pending = vec![oauth_error("authorization_pending"); 3];
    let service = service(device(None, 12), pending).await;
    let start = Instant::now();
    let authorization: DeviceAuthorization =
        serde_json::from_value(device(None, 12)).expect("a device authorization without interval");
    let result = poll_device_token(&client(), &service.server.url, &authorization, &CancellationToken::new()).await;
    assert_eq!(result, Err(PiRustError::Failed(EXPIRED.into())));
    // Without `interval` the client waits 5 seconds between polls.
    let times: Vec<u128> = service.poll_times.lock().unwrap().iter().map(|at| (*at - start).as_millis()).collect();
    assert_eq!(times, [5000, 10000]);
}

#[tokio::test(start_paused = true)]
async fn cancelling_stops_the_sign_in() {
    let service = service(device(Some(5), 600), Vec::new()).await;
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        trigger.cancel();
    });
    let result = sign_in(&client(), &service.server.url, &Recorder::default(), &cancel).await;
    assert_eq!(result, Err(PiRustError::Cancelled));
    assert_eq!(service.server.count("/oauth/token"), 0);
}

#[tokio::test]
async fn device_authorization_errors_use_the_service_description() {
    let server = TestServer::start(|_| {
        reply(429, json!({"ok": false, "error": "rate_limited", "description": "Too many sign-ins"}))
    })
    .await;
    let error = start_device_authorization(&client(), &server.url, &CancellationToken::new()).await.unwrap_err();
    assert_eq!(error.to_string(), "pi-rust.com sign-in failed: Too many sign-ins");
    let mut insecure = device(Some(1), 600);
    insecure["verification_uri"] = json!("file:///etc/passwd");
    let server = TestServer::start(move |_| reply(200, insecure.clone())).await;
    let error = start_device_authorization(&client(), &server.url, &CancellationToken::new()).await.unwrap_err();
    assert_eq!(error.to_string(), "pi-rust.com sign-in failed: invalid device authorization response");
}

fn account(origin: &str) -> PiRustAccount {
    PiRustAccount { origin: origin.into(), token: "tok-secret".into(), email: Some("me@share.test".into()) }
}

#[tokio::test]
async fn uploads_the_html_with_the_title() {
    let server = TestServer::start(|_| {
        let share =
            json!({"id": "s1", "url": "https://share.test/s/s1", "title": "My session", "size": 5, "createdAt": "now"});
        reply(201, json!({"ok": true, "share": share}))
    })
    .await;
    let html = b"<html>session</html>".to_vec();
    let share =
        upload_share(&client(), &account(&server.url), "My session & more", html.clone(), &CancellationToken::new())
            .await
            .unwrap();
    assert_eq!((share.id.as_str(), share.url.as_str()), ("s1", "https://share.test/s/s1"));
    let request = &server.requests()[0];
    assert_eq!((request.method.as_str(), request.path()), ("POST", "/v1/shares"));
    assert_eq!(request.query("title").as_deref(), Some("My session & more"));
    assert_eq!(request.header("authorization"), Some("Bearer tok-secret"));
    assert_eq!(request.header("content-type"), Some("text/html; charset=utf-8"));
    assert_eq!(request.body, html);
}

#[tokio::test]
async fn upload_errors_report_revoked_tokens_and_limits() {
    let unauthorized = TestServer::start(|_| {
        reply(401, json!({"ok": false, "error": "unauthorized", "description": "Token revoked"}))
    })
    .await;
    let cancel = CancellationToken::new();
    let result = upload_share(&client(), &account(&unauthorized.url), "t", b"x".to_vec(), &cancel).await;
    assert_eq!(result, Err(PiRustError::Unauthorized));
    assert!(PiRustError::Unauthorized.to_string().contains("Run /share again to sign in"));

    let too_large = TestServer::start(|_| {
        reply(413, json!({"ok": false, "error": "too_large", "description": "Shares are limited to 25 MiB"}))
    })
    .await;
    let error = upload_share(&client(), &account(&too_large.url), "t", b"x".to_vec(), &cancel).await.unwrap_err();
    assert_eq!(error.to_string(), "Failed to share on pi-rust.com: Shares are limited to 25 MiB");
    let limited = TestServer::start(|_| reply(429, json!({"ok": false, "error": "rate_limited"}))).await;
    let error = upload_share(&client(), &account(&limited.url), "t", b"x".to_vec(), &cancel).await.unwrap_err();
    assert_eq!(error.to_string(), "Failed to share on pi-rust.com: rate_limited");

    let local = TestServer::start(|_| panic!("an oversized export is not sent")).await;
    let html = vec![b' '; MAX_SHARE_BYTES + 1];
    let error = upload_share(&client(), &account(&local.url), "t", html, &cancel).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "Failed to share on pi-rust.com: the session export is 25.0 MiB, more than the 25 MiB limit"
    );
}

#[tokio::test]
async fn reads_the_account_and_revokes_the_token() {
    let server = TestServer::start(|request| match (request.method.as_str(), request.path()) {
        ("GET", "/v1/account") => reply(200, json!({"ok": true, "account": {"email": "me@share.test"}})),
        ("DELETE", "/v1/token") => TestReply::status(204, ""),
        other => panic!("unexpected {other:?}"),
    })
    .await;
    let cancel = CancellationToken::new();
    let email = fetch_account_email(&client(), &server.url, "tok-secret", &cancel).await.unwrap();
    assert_eq!(email.as_deref(), Some("me@share.test"));
    revoke_token(&client(), &account(&server.url)).await.unwrap();
    let revoke = &server.requests()[1];
    assert_eq!(revoke.header("authorization"), Some("Bearer tok-secret"));

    let revoked = TestServer::start(|_| reply(401, json!({"ok": false, "error": "unauthorized"}))).await;
    let result = fetch_account_email(&client(), &revoked.url, "tok", &cancel).await;
    assert_eq!(result, Err(PiRustError::Unauthorized));
    revoke_token(&client(), &account(&revoked.url)).await.expect("an unknown token counts as revoked");
    let broken = TestServer::start(|_| TestReply::status(500, "oops")).await;
    let error = revoke_token(&client(), &account(&broken.url)).await.unwrap_err();
    assert_eq!(error.to_string(), "Could not revoke the pi-rust.com token: Internal Server Error");
}

#[test]
fn stores_the_account_privately_per_origin() {
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path().join("agent");
    assert_eq!(load_account(&agent_dir), None);
    let stored = account("https://pi-rust.com");
    save_account(&agent_dir, &stored).unwrap();
    assert_eq!(load_account(&agent_dir), Some(stored.clone()));
    assert_eq!(load_account_for(&agent_dir, "https://pi-rust.com/"), Some(stored.clone()));
    assert_eq!(load_account_for(&agent_dir, "http://127.0.0.1:9"), None);
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(agent_dir.join(ACCOUNT_FILE)).unwrap()).unwrap();
    assert_eq!(saved, json!({"origin": "https://pi-rust.com", "token": "tok-secret", "email": "me@share.test"}));
    assert!(!format!("{stored:?}").contains("tok-secret"), "Debug hides the token");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&agent_dir.join(ACCOUNT_FILE)), 0o600);
        assert_eq!(mode(&agent_dir), 0o700);
        std::fs::set_permissions(agent_dir.join(ACCOUNT_FILE), std::fs::Permissions::from_mode(0o644)).unwrap();
        save_account(&agent_dir, &stored).unwrap();
        assert_eq!(mode(&agent_dir.join(ACCOUNT_FILE)), 0o600, "an existing file is narrowed");
    }
    delete_account(&agent_dir).unwrap();
    assert_eq!(load_account(&agent_dir), None);
    delete_account(&agent_dir).expect("a missing file is already deleted");
}
