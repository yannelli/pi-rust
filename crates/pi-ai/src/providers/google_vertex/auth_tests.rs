use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use super::{AccessToken, credentials_token};
use crate::providers::aws::sigv4::format_amz_date;
use crate::providers::mock_http::{MockHttp, Reply};

pub(super) const WORKFORCE_AUDIENCE: &str = "//iam.googleapis.com/locations/global/workforcePools/pool/providers/prov";
pub(super) const WORKLOAD_AUDIENCE: &str =
    "//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/pool/providers/prov";
pub(super) const ID_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:id_token";

pub(super) fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(name, value)| (name.to_string(), value.to_string())).collect()
}

pub(super) async fn token(credentials: &Value, env: &BTreeMap<String, String>) -> Result<(AccessToken, i64), String> {
    credentials_token(credentials, env).await
}

pub(super) fn sts_reply(token: &str) -> Reply {
    let body = json!({"access_token": token, "issued_token_type": "urn:ietf:params:oauth:token-type:access_token",
        "token_type": "Bearer", "expires_in": 3600});
    Reply::json(200, &body)
}

/// An RFC 3339 timestamp `seconds` from now.
fn rfc3339_in(seconds: u64) -> String {
    let amz = format_amz_date(SystemTime::now() + Duration::from_secs(seconds));
    format!("{}-{}-{}T{}:{}:{}Z", &amz[0..4], &amz[4..6], &amz[6..8], &amz[9..11], &amz[11..13], &amz[13..15])
}

fn file_source_credentials(server: &MockHttp, subject_file: &std::path::Path) -> Value {
    json!({
        "type": "external_account", "audience": WORKFORCE_AUDIENCE, "subject_token_type": ID_TOKEN_TYPE,
        "token_url": format!("{}/v1/token", server.base_url), "workforce_pool_user_project": "user-project",
        "credential_source": {"file": subject_file.display().to_string()}, "quota_project_id": "quota"
    })
}

#[tokio::test]
async fn identity_pool_files_exchange_text_tokens_at_sts() {
    let server = MockHttp::start(vec![sts_reply("sts-token"), sts_reply("sts-token-2")]).await;
    let dir = tempfile::tempdir().unwrap();
    let subject = dir.path().join("token.txt");
    std::fs::write(&subject, "file-subject-token").unwrap();
    let credentials = file_source_credentials(&server, &subject);
    let (access, expires_in) = token(&credentials, &env(&[])).await.unwrap();
    assert_eq!(access, AccessToken { token: "sts-token".into(), quota_project: Some("quota".into()) });
    assert_eq!(expires_in, 3600);
    let request = server.request(0);
    assert_eq!((request.method.as_str(), request.path.as_str()), ("POST", "/v1/token"));
    assert!(request.headers["x-goog-api-client"].contains("source/file sa-impersonation/false config-lifetime/false"));
    assert!(!request.headers.contains_key("authorization"));
    let form = request.form();
    assert_eq!(form["grant_type"], "urn:ietf:params:oauth:grant-type:token-exchange");
    assert_eq!(form["audience"], WORKFORCE_AUDIENCE);
    assert_eq!(form["scope"], "https://www.googleapis.com/auth/cloud-platform");
    assert_eq!(form["requested_token_type"], "urn:ietf:params:oauth:token-type:access_token");
    assert_eq!(form["subject_token"], "file-subject-token");
    assert_eq!(form["subject_token_type"], ID_TOKEN_TYPE);
    assert_eq!(serde_json::from_str::<Value>(&form["options"]).unwrap(), json!({"userProject": "user-project"}));

    let mut confidential = credentials.clone();
    confidential["client_id"] = json!("cid");
    confidential["client_secret"] = json!("secret");
    token(&confidential, &env(&[])).await.unwrap();
    let request = server.request(1);
    assert_eq!(request.headers["authorization"], format!("Basic {}", STANDARD.encode("cid:secret")));
    assert!(!request.form().contains_key("options"), "client auth drops the userProject option");

    let mut workload = credentials.clone();
    workload["audience"] = json!(WORKLOAD_AUDIENCE);
    let error = token(&workload, &env(&[])).await.unwrap_err();
    assert_eq!(error, "workforcePoolUserProject should not be set for non-workforce pool credentials.");
}

#[tokio::test]
async fn identity_pool_files_read_json_subject_token_fields() {
    let server = MockHttp::start(vec![sts_reply("sts-json")]).await;
    let dir = tempfile::tempdir().unwrap();
    let subject = dir.path().join("token.json");
    std::fs::write(&subject, json!({"id_token": "json-subject", "other": 1}).to_string()).unwrap();
    let mut credentials = file_source_credentials(&server, &subject);
    credentials.as_object_mut().unwrap().remove("workforce_pool_user_project");
    credentials["audience"] = json!(WORKLOAD_AUDIENCE);
    credentials["credential_source"]["format"] = json!({"type": "json", "subject_token_field_name": "id_token"});
    let (access, _) = token(&credentials, &env(&[])).await.unwrap();
    assert_eq!(access.token, "sts-json");
    let form = server.request(0).form();
    assert_eq!(form["subject_token"], "json-subject");
    assert!(!form.contains_key("options"));

    credentials["credential_source"]["format"]["subject_token_field_name"] = json!("missing");
    let error = token(&credentials, &env(&[])).await.unwrap_err();
    assert_eq!(error, "Unable to parse the subject_token from the credential_source file");
    credentials["credential_source"]["format"] = json!({"type": "json"});
    let error = token(&credentials, &env(&[])).await.unwrap_err();
    assert_eq!(error, "Missing subject_token_field_name for JSON credential_source format");
    credentials["credential_source"] = json!({"file": dir.path().join("absent").display().to_string()});
    assert!(token(&credentials, &env(&[])).await.unwrap_err().contains("does not exist, or it is not a file."));
    credentials.as_object_mut().unwrap().remove("credential_source");
    let error = token(&credentials, &env(&[])).await.unwrap_err();
    assert_eq!(error, "A credential source or subject token supplier must be specified.");
}

#[tokio::test]
async fn sts_errors_use_the_oauth_error_format() {
    let body = json!({"error": "invalid_grant", "error_description": "bad subject", "error_uri": "https://e"});
    let server = MockHttp::start(vec![Reply::json(400, &body)]).await;
    let dir = tempfile::tempdir().unwrap();
    let subject = dir.path().join("token.txt");
    std::fs::write(&subject, "t").unwrap();
    let error = token(&file_source_credentials(&server, &subject), &env(&[])).await.unwrap_err();
    assert_eq!(error, "Error code invalid_grant: bad subject - https://e");
}

#[tokio::test]
async fn service_account_impersonation_follows_the_sts_exchange() {
    let impersonated = json!({"accessToken": "impersonated-token", "expireTime": rfc3339_in(1800)});
    let server = MockHttp::start(vec![sts_reply("sts-token"), Reply::json(200, &impersonated)]).await;
    let dir = tempfile::tempdir().unwrap();
    let subject = dir.path().join("token.txt");
    std::fs::write(&subject, "subject").unwrap();
    let mut credentials = file_source_credentials(&server, &subject);
    let url =
        format!("{}/v1/projects/-/serviceAccounts/sa@p.iam.gserviceaccount.com:generateAccessToken", server.base_url);
    credentials["service_account_impersonation_url"] = json!(url);
    credentials["service_account_impersonation"] = json!({"token_lifetime_seconds": 2400});
    let (access, expires_in) = token(&credentials, &env(&[])).await.unwrap();
    assert_eq!(access.token, "impersonated-token");
    assert!((1790..=1800).contains(&expires_in), "{expires_in}");
    let sts = server.request(0);
    assert!(sts.headers["x-goog-api-client"].ends_with("sa-impersonation/true config-lifetime/true"));
    assert_eq!(sts.form()["scope"], "https://www.googleapis.com/auth/cloud-platform");
    let request = server.request(1);
    assert_eq!(request.path, "/v1/projects/-/serviceAccounts/sa@p.iam.gserviceaccount.com:generateAccessToken");
    assert_eq!(request.headers["authorization"], "Bearer sts-token");
    assert_eq!(
        request.json(),
        json!({"scope": ["https://www.googleapis.com/auth/cloud-platform"], "lifetime": "2400s"})
    );
}

#[tokio::test]
async fn impersonated_service_accounts_use_their_authorized_user_source() {
    let user_token = json!({"access_token": "user-token", "expires_in": 3599});
    let generated = json!({"accessToken": "target-token", "expireTime": rfc3339_in(3600)});
    let denied = json!({"error": {"code": 403, "status": "PERMISSION_DENIED", "message": "denied"}});
    let server = MockHttp::start(vec![
        Reply::json(200, &user_token),
        Reply::json(200, &generated),
        Reply::json(200, &user_token),
        Reply::json(403, &denied),
    ])
    .await;
    let credentials = json!({
        "type": "impersonated_service_account",
        "service_account_impersonation_url": "https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/target@p.iam.gserviceaccount.com:generateAccessToken",
        "endpoint": server.base_url, "delegates": ["delegate@p.iam.gserviceaccount.com"], "quota_project_id": "top-quota",
        "source_credentials": {
            "type": "authorized_user", "client_id": "cid", "client_secret": "secret", "refresh_token": "refresh",
            "token_uri": format!("{}/token", server.base_url), "quota_project_id": "source-quota"
        }
    });
    let (access, expires_in) = token(&credentials, &env(&[])).await.unwrap();
    assert_eq!(access, AccessToken { token: "target-token".into(), quota_project: Some("top-quota".into()) });
    assert!((3590..=3600).contains(&expires_in), "{expires_in}");
    assert_eq!(server.request(0).form()["grant_type"], "refresh_token");
    let request = server.request(1);
    assert_eq!(request.path, "/v1/projects/-/serviceAccounts/target@p.iam.gserviceaccount.com:generateAccessToken");
    assert_eq!(request.headers["authorization"], "Bearer user-token");
    assert_eq!(request.headers["x-goog-user-project"], "source-quota");
    let body = json!({"delegates": ["delegate@p.iam.gserviceaccount.com"],
        "scope": ["https://www.googleapis.com/auth/cloud-platform"], "lifetime": "3600s"});
    assert_eq!(request.json(), body);

    let error = token(&credentials, &env(&[])).await.unwrap_err();
    assert_eq!(error, "PERMISSION_DENIED: unable to impersonate: denied");
    let mut missing = credentials.clone();
    missing.as_object_mut().unwrap().remove("source_credentials");
    let error = token(&missing, &env(&[])).await.unwrap_err();
    assert_eq!(error, "The incoming JSON object does not contain a source_credentials field");
}

#[tokio::test]
async fn external_authorized_users_refresh_with_basic_auth_and_rotate_tokens() {
    let first = json!({"access_token": "ext-user-1", "expires_in": 1200, "refresh_token": "rotated-refresh"});
    let second = json!({"access_token": "ext-user-2", "expires_in": 1200});
    let server = MockHttp::start(vec![Reply::json(200, &first), Reply::json(200, &second)]).await;
    let credentials = json!({
        "type": "external_account_authorized_user", "audience": WORKFORCE_AUDIENCE, "client_id": "cid",
        "client_secret": "secret", "refresh_token": "ext-refresh-original", "quota_project_id": "q",
        "token_url": format!("{}/v1/oauthtoken", server.base_url), "token_info_url": "unused"
    });
    let (access, expires_in) = token(&credentials, &env(&[])).await.unwrap();
    assert_eq!((access.token.as_str(), access.quota_project.as_deref(), expires_in), ("ext-user-1", Some("q"), 1200));
    let request = server.request(0);
    assert_eq!(request.path, "/v1/oauthtoken");
    assert_eq!(request.headers["authorization"], format!("Basic {}", STANDARD.encode("cid:secret")));
    let form = request.form();
    assert_eq!(
        (form["grant_type"].as_str(), form["refresh_token"].as_str()),
        ("refresh_token", "ext-refresh-original")
    );
    token(&credentials, &env(&[])).await.unwrap();
    assert_eq!(server.request(1).form()["refresh_token"], "rotated-refresh");
}
