//! SSO token refresh and MFA prompts for assume-role profiles, against local mock servers.

use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::json;

use super::tests::{Fixture, STS_REPLY, fixture, input};
use super::*;
use crate::providers::aws::sigv4::format_iso8601;
use crate::providers::http_mock::{Recorded, Reply, RouteServer};

const REFRESH: &str = "To refresh this SSO session run 'aws sso login' with the corresponding profile.";

fn iso_in(seconds: i64) -> String {
    let now = SystemTime::now();
    let delta = Duration::from_secs(seconds.unsigned_abs());
    format_iso8601(if seconds >= 0 { now + delta } else { now - delta })
}

/// Serves `CreateToken` with `token_reply` and role credentials that expire in a minute,
/// so the chain cache does not answer the next call.
async fn sso_server(token_reply: Reply) -> RouteServer {
    RouteServer::start(move |request: &Recorded| {
        if request.path == "/token" {
            return token_reply.clone();
        }
        let expiration = (SystemTime::now() + Duration::from_secs(60)).duration_since(UNIX_EPOCH).unwrap();
        Reply::json(
            200,
            &json!({"roleCredentials": {"accessKeyId": "ASIASSO", "secretAccessKey": "sso-secret", "sessionToken": "sso-token", "expiration": expiration.as_millis() as u64}}),
        )
    })
    .await
}

/// A profile on `sso-session {session}` with both SSO endpoints pointed at `server`.
fn sso_fixture(profile: &str, session: &str, server: &RouteServer) -> Fixture {
    let config = format!(
        "[profile {profile}]\nsso_session = {session}\nsso_account_id = 123456789012\nsso_role_name = Dev\n[sso-session {session}]\nsso_region = us-east-1\nsso_start_url = https://corp/start\n"
    );
    let mut fixture = fixture(&config, "");
    fixture.env.insert("AWS_ENDPOINT_URL_SSO_OIDC".into(), server.base_url.clone());
    fixture.env.insert("AWS_ENDPOINT_URL_SSO".into(), server.base_url.clone());
    fixture
}

fn write_token(fixture: &Fixture, id: &str, token: &Value) -> std::path::PathBuf {
    let path = sso::token_filepath(id, &fixture.env);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_string_pretty(token).unwrap()).unwrap();
    path
}

fn refreshable_token(expires_in: i64) -> Value {
    json!({
        "startUrl": "https://corp/start",
        "region": "us-east-1",
        "accessToken": "old-token",
        "expiresAt": iso_in(expires_in),
        "clientId": "client-id",
        "clientSecret": "client-secret",
        "registrationExpiresAt": "2099-01-01T00:00:00Z",
        "refreshToken": "refresh-1",
    })
}

fn bearer_tokens(server: &RouteServer) -> Vec<String> {
    let requests = server.requests();
    let portal = requests.iter().filter(|request| request.path.starts_with("/federation/credentials"));
    portal.map(|request| request.headers["x-amz-sso_bearer_token"].clone()).collect()
}

fn token_calls(server: &RouteServer) -> usize {
    server.requests().iter().filter(|request| request.path == "/token").count()
}

#[tokio::test]
async fn refreshes_near_expiry_tokens_and_writes_the_cache_file() {
    let reply =
        json!({"accessToken": "new-token", "expiresIn": 3600, "refreshToken": "refresh-2", "tokenType": "Bearer"});
    let server = sso_server(Reply::json(200, &reply)).await;
    let fixture = sso_fixture("sso-refresh", "corp-refresh", &server);
    let path = write_token(&fixture, "corp-refresh", &refreshable_token(60));
    let credentials = resolve(&input(&fixture.env, Some("sso-refresh"))).await.unwrap();
    assert_eq!(credentials.access_key_id, "ASIASSO");

    let requests = server.requests();
    assert_eq!((requests[0].method.as_str(), requests[0].path.as_str()), ("POST", "/token"));
    let body = json!({"clientId": "client-id", "clientSecret": "client-secret", "grantType": "refresh_token", "refreshToken": "refresh-1"});
    assert_eq!(requests[0].json(), body);
    assert_eq!(requests[1].path, "/federation/credentials?account_id=123456789012&role_name=Dev");
    assert_eq!(bearer_tokens(&server), ["new-token"]);

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("{\n  \"startUrl\": \"https://corp/start\",\n"), "{text}");
    let saved: Value = serde_json::from_str(&text).unwrap();
    let keys: Vec<&str> = saved.as_object().unwrap().keys().map(String::as_str).collect();
    let expected = [
        "startUrl",
        "region",
        "accessToken",
        "expiresAt",
        "clientId",
        "clientSecret",
        "registrationExpiresAt",
        "refreshToken",
    ];
    assert_eq!(keys, expected);
    assert_eq!((saved["accessToken"].as_str(), saved["refreshToken"].as_str()), (Some("new-token"), Some("refresh-2")));
    assert_eq!(saved["clientSecret"], "client-secret");
    let expires = parse_rfc3339(saved["expiresAt"].as_str().unwrap()).unwrap();
    let left = expires.duration_since(SystemTime::now()).unwrap().as_secs();
    assert!((3590..=3600).contains(&left), "{left}");
}

#[tokio::test]
async fn throttles_refreshes_and_falls_back_to_a_valid_token() {
    let server = sso_server(Reply::json(500, &json!({"error": "server_error"}))).await;
    let fixture = sso_fixture("sso-throttle", "corp-throttle", &server);
    let token = refreshable_token(120);
    let path = write_token(&fixture, "corp-throttle", &token);
    for _ in 0..2 {
        let credentials = resolve(&input(&fixture.env, Some("sso-throttle"))).await.unwrap();
        assert_eq!(credentials.access_key_id, "ASIASSO");
    }
    assert_eq!(token_calls(&server), 1);
    assert_eq!(bearer_tokens(&server), ["old-token", "old-token"]);
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(saved, token);
}

#[tokio::test]
async fn expired_tokens_fail_when_the_refresh_fails() {
    let server = sso_server(Reply::json(400, &json!({"error": "invalid_grant"}))).await;
    let fixture = sso_fixture("sso-expired", "corp-expired", &server);
    write_token(&fixture, "corp-expired", &refreshable_token(-60));
    for _ in 0..2 {
        let error = resolve(&input(&fixture.env, Some("sso-expired"))).await.unwrap_err();
        assert!(error.contains(&format!("Token is expired. {REFRESH}")), "{error}");
    }
    assert_eq!(token_calls(&server), 1);
    assert!(bearer_tokens(&server).is_empty());
}

#[tokio::test]
async fn reports_missing_session_tokens() {
    let server = sso_server(Reply::json(500, &json!({}))).await;
    let fixture = sso_fixture("sso-missing", "corp-missing", &server);
    let error = resolve(&input(&fixture.env, Some("sso-missing"))).await.unwrap_err();
    let expected =
        format!("The SSO session token associated with profile=sso-missing was not found or is invalid. {REFRESH}");
    assert!(error.contains(&expected), "{error}");
}

#[tokio::test]
async fn legacy_profiles_use_the_cached_token_without_refreshing() {
    let server = sso_server(Reply::json(500, &json!({}))).await;
    let config = "[profile sso-legacy]\nsso_start_url = https://legacy/start\nsso_region = us-east-1\nsso_account_id = 1\nsso_role_name = Dev\n";
    let mut fixture = fixture(config, "");
    fixture.env.insert("AWS_ENDPOINT_URL".into(), server.base_url.clone());
    write_token(&fixture, "https://legacy/start", &refreshable_token(60));
    let credentials = resolve(&input(&fixture.env, Some("sso-legacy"))).await.unwrap();
    assert_eq!(credentials.access_key_id, "ASIASSO");
    assert_eq!(token_calls(&server), 0);
    assert_eq!(bearer_tokens(&server), ["old-token"]);
}

const MFA_CONFIG: &str = "[profile mfa-app]\nrole_arn = arn:aws:iam::123:role/app\nsource_profile = base\nmfa_serial = arn:aws:iam::123:mfa/me\n[profile mfa-none]\nrole_arn = arn:aws:iam::123:role/none\nsource_profile = base\nmfa_serial = arn:aws:iam::123:mfa/me\n[profile mfa-global]\nrole_arn = arn:aws:iam::123:role/global\nsource_profile = base\nmfa_serial = arn:aws:iam::123:mfa/global\n[profile mfa-cancel]\nrole_arn = arn:aws:iam::123:role/cancel\nsource_profile = base\nmfa_serial = arn:aws:iam::123:mfa/me\n";
const BASE_KEYS: &str = "[base]\naws_access_key_id = AKIABASE\naws_secret_access_key = base-secret\n";

async fn mfa_fixture() -> (RouteServer, Fixture) {
    let server = RouteServer::start(|_| Reply::new(200, "text/xml", STS_REPLY)).await;
    let mut fixture = fixture(MFA_CONFIG, BASE_KEYS);
    fixture.env.insert("AWS_ENDPOINT_URL_STS".into(), server.base_url.clone());
    (server, fixture)
}

fn answer(code: &'static str, prompts: Arc<Mutex<Vec<MfaPrompt>>>) -> MfaCodeProvider {
    Arc::new(move |prompt| {
        prompts.lock().unwrap().push(prompt);
        Box::pin(async move { Ok(code.to_string()) })
    })
}

#[tokio::test]
async fn mfa_callback_codes_go_into_assume_role_and_are_cached() {
    let (server, fixture) = mfa_fixture().await;
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let mut chain = input(&fixture.env, Some("mfa-app"));
    chain.mfa_code_provider = Some(answer("123456", prompts.clone()));
    for _ in 0..2 {
        assert_eq!(resolve(&chain).await.unwrap().access_key_id, "ASIAROLE");
    }
    let expected = MfaPrompt { profile: "mfa-app".into(), serial: "arn:aws:iam::123:mfa/me".into() };
    assert_eq!(*prompts.lock().unwrap(), [expected]);
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let body = String::from_utf8_lossy(&requests[0].body);
    assert!(body.contains("&SerialNumber=arn%3Aaws%3Aiam%3A%3A123%3Amfa%2Fme&TokenCode=123456"), "{body}");
}

#[tokio::test]
async fn mfa_profiles_need_a_callback_and_fall_back_to_the_global_one() {
    let (server, fixture) = mfa_fixture().await;
    let error = resolve(&input(&fixture.env, Some("mfa-none"))).await.unwrap_err();
    let expected = "Profile mfa-none requires multi-factor authentication, but no MFA code callback was provided.";
    assert!(error.contains(expected), "{error}");
    assert!(server.requests().is_empty());

    let prompts = Arc::new(Mutex::new(Vec::new()));
    set_mfa_code_provider(Some(answer("654321", prompts.clone())));
    let result = resolve(&input(&fixture.env, Some("mfa-global"))).await;
    set_mfa_code_provider(None);
    assert_eq!(result.unwrap().access_key_id, "ASIAROLE");
    assert_eq!(prompts.lock().unwrap()[0].serial, "arn:aws:iam::123:mfa/global");
    let body = String::from_utf8_lossy(&server.requests()[0].body).into_owned();
    assert!(body.contains("TokenCode=654321"), "{body}");
}

#[tokio::test]
async fn mfa_callback_errors_stop_the_role_assumption() {
    let (server, fixture) = mfa_fixture().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let mut chain = input(&fixture.env, Some("mfa-cancel"));
    chain.mfa_code_provider = Some(Arc::new(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err("MFA prompt cancelled".to_string()) })
    }));
    let error = resolve(&chain).await.unwrap_err();
    assert!(error.contains("MFA prompt cancelled"), "{error}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(server.requests().is_empty());
}
