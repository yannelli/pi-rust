use std::collections::BTreeMap;

use serde_json::json;

use super::*;
use crate::providers::http_mock::{Reply, RouteServer};

const STS_REPLY: &str = "<AssumeRoleResponse><AssumeRoleResult><Credentials><AccessKeyId>ASIAROLE</AccessKeyId><SecretAccessKey>role&amp;secret</SecretAccessKey><SessionToken>role-token</SessionToken><Expiration>2099-01-01T00:00:00Z</Expiration></Credentials></AssumeRoleResult></AssumeRoleResponse>";

struct Fixture {
    dir: tempfile::TempDir,
    env: BTreeMap<String, String>,
}

/// Config files in a temp home, with instance metadata disabled.
fn fixture(config: &str, credentials: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config"), config).unwrap();
    std::fs::write(dir.path().join("credentials"), credentials).unwrap();
    let path = |name: &str| dir.path().join(name).display().to_string();
    let env = BTreeMap::from([
        ("HOME".to_string(), dir.path().display().to_string()),
        ("AWS_CONFIG_FILE".to_string(), path("config")),
        ("AWS_SHARED_CREDENTIALS_FILE".to_string(), path("credentials")),
        ("AWS_EC2_METADATA_DISABLED".to_string(), "true".to_string()),
    ]);
    Fixture { dir, env }
}

fn input<'a>(env: &'a BTreeMap<String, String>, profile: Option<&str>) -> ChainInput<'a> {
    ChainInput { env, profile: profile.map(str::to_string), region: "us-east-1".into() }
}

#[tokio::test]
async fn environment_keys_win_unless_a_request_profile_is_set() {
    let mut fixture = fixture("", "[dev]\naws_access_key_id = AKIADEV\naws_secret_access_key = dev-secret\n");
    fixture.env.insert("AWS_ACCESS_KEY_ID".into(), "AKIAENV".into());
    fixture.env.insert("AWS_SECRET_ACCESS_KEY".into(), "env-secret".into());
    fixture.env.insert("AWS_SESSION_TOKEN".into(), "env-token".into());
    let ambient = resolve(&input(&fixture.env, None)).await.unwrap();
    assert_eq!((ambient.access_key_id.as_str(), ambient.session_token.as_deref()), ("AKIAENV", Some("env-token")));
    let scoped = resolve(&input(&fixture.env, Some("dev"))).await.unwrap();
    assert_eq!((scoped.access_key_id.as_str(), scoped.secret_access_key.as_str()), ("AKIADEV", "dev-secret"));
}

#[tokio::test]
async fn assumes_roles_through_source_profiles_with_signed_sts_calls() {
    let server = RouteServer::start(|_| Reply::new(200, "text/xml", STS_REPLY)).await;
    let config = "[profile app]\nrole_arn = arn:aws:iam::123:role/app\nsource_profile = base\nregion = eu-west-1\nexternal_id = ext 1\n";
    let mut fixture = fixture(config, "[base]\naws_access_key_id = AKIABASE\naws_secret_access_key = base-secret\n");
    fixture.env.insert("AWS_ENDPOINT_URL_STS".into(), format!("{}/", server.base_url));
    let credentials = resolve(&input(&fixture.env, Some("app"))).await.unwrap();
    assert_eq!(credentials.access_key_id, "ASIAROLE");
    assert_eq!(credentials.secret_access_key, "role&secret");
    assert_eq!(credentials.session_token.as_deref(), Some("role-token"));
    assert!(credentials.expiration.is_some());
    let request = &server.requests()[0];
    let body = String::from_utf8_lossy(&request.body);
    assert!(
        body.contains("Action=AssumeRole&") && body.contains("RoleArn=arn%3Aaws%3Aiam%3A%3A123%3Arole%2Fapp"),
        "{body}"
    );
    assert!(body.contains("ExternalId=ext%201"), "{body}");
    let authorization = &request.headers["authorization"];
    assert!(authorization.contains("Credential=AKIABASE/") && authorization.contains("/eu-west-1/sts/aws4_request"));
}

#[tokio::test]
async fn detects_source_profile_cycles() {
    let config = "[profile a]\nrole_arn = arn:aws:iam::1:role/a\nsource_profile = b\n[profile b]\nrole_arn = arn:aws:iam::1:role/b\nsource_profile = a\n";
    let fixture = fixture(config, "");
    let error = resolve(&input(&fixture.env, Some("a"))).await.unwrap_err();
    assert!(error.contains("Detected a cycle"), "{error}");
}

#[cfg(unix)]
#[tokio::test]
async fn runs_credential_process() {
    let output = r#"{"Version":1,"AccessKeyId":"AKIAPROC","SecretAccessKey":"proc-secret","SessionToken":"proc-token","Expiration":"2099-01-01T00:00:00Z"}"#;
    let config = format!("[profile proc]\ncredential_process = printf '%s' '{output}'\n");
    let fixture = fixture(&config, "");
    let credentials = resolve(&input(&fixture.env, Some("proc"))).await.unwrap();
    assert_eq!(
        (credentials.access_key_id.as_str(), credentials.session_token.as_deref()),
        ("AKIAPROC", Some("proc-token"))
    );
    let bad = fixture_with_process("printf '{\"Version\":2}'");
    let error = resolve(&input(&bad.env, Some("proc"))).await.unwrap_err();
    assert!(error.contains("Profile proc credential_process did not return Version 1."), "{error}");
}

#[cfg(unix)]
fn fixture_with_process(command: &str) -> Fixture {
    fixture(&format!("[profile proc]\ncredential_process = {command}\n"), "")
}

#[tokio::test]
async fn reads_container_credentials_with_the_authorization_token() {
    let body = json!({"AccessKeyId": "AKIAECS", "SecretAccessKey": "ecs-secret", "Token": "ecs-token", "Expiration": "2099-01-01T00:00:00Z"});
    let server = RouteServer::start(move |_| Reply::json(200, &body)).await;
    let mut fixture = fixture("", "");
    fixture.env.insert("AWS_CONTAINER_CREDENTIALS_FULL_URI".into(), format!("{}/creds", server.base_url));
    fixture.env.insert("AWS_CONTAINER_AUTHORIZATION_TOKEN".into(), "Bearer pod".into());
    let credentials = resolve(&input(&fixture.env, None)).await.unwrap();
    assert_eq!(
        (credentials.access_key_id.as_str(), credentials.session_token.as_deref()),
        ("AKIAECS", Some("ecs-token"))
    );
    let request = &server.requests()[0];
    assert_eq!((request.path.as_str(), request.headers["authorization"].as_str()), ("/creds", "Bearer pod"));
}

#[tokio::test]
async fn reads_instance_metadata_with_an_imdsv2_token() {
    let server = RouteServer::start(|request| match request.path.as_str() {
        "/latest/api/token" => Reply::new(200, "text/plain", "imds-token"),
        "/latest/meta-data/iam/security-credentials/" => Reply::new(200, "text/plain", "web-role\n"),
        _ => Reply::json(200, &json!({"AccessKeyId": "AKIAEC2", "SecretAccessKey": "ec2-secret", "Token": "ec2-token", "Expiration": "2099-01-01T00:00:00Z"})),
    })
    .await;
    let mut fixture = fixture("", "");
    fixture.env.insert("AWS_EC2_METADATA_DISABLED".into(), "false".into());
    fixture.env.insert("AWS_EC2_METADATA_SERVICE_ENDPOINT".into(), server.base_url.clone());
    let credentials = resolve(&input(&fixture.env, None)).await.unwrap();
    assert_eq!(credentials.access_key_id, "AKIAEC2");
    let requests = server.requests();
    assert_eq!(requests[0].method, "PUT");
    assert_eq!(requests[2].path, "/latest/meta-data/iam/security-credentials/web-role");
    assert_eq!(requests[2].headers["x-aws-ec2-metadata-token"], "imds-token");
}

#[tokio::test]
async fn exchanges_web_identity_tokens() {
    let server = RouteServer::start(|_| Reply::new(200, "text/xml", STS_REPLY)).await;
    let mut fixture = fixture("", "");
    let token_file = fixture.dir.path().join("token");
    std::fs::write(&token_file, "jwt-token\n").unwrap();
    fixture.env.insert("AWS_WEB_IDENTITY_TOKEN_FILE".into(), token_file.display().to_string());
    fixture.env.insert("AWS_ROLE_ARN".into(), "arn:aws:iam::1:role/web".into());
    fixture.env.insert("AWS_ROLE_SESSION_NAME".into(), "pi".into());
    fixture.env.insert("AWS_ENDPOINT_URL_STS".into(), server.base_url.clone());
    let credentials = resolve(&input(&fixture.env, None)).await.unwrap();
    assert_eq!(credentials.access_key_id, "ASIAROLE");
    let request = &server.requests()[0];
    let body = String::from_utf8_lossy(&request.body);
    assert!(body.contains("Action=AssumeRoleWithWebIdentity") && body.contains("WebIdentityToken=jwt-token"), "{body}");
    assert!(!request.headers.contains_key("authorization"));
}

#[tokio::test]
async fn uses_sso_and_login_caches() {
    let config = "[profile sso]\nsso_session = corp\nsso_account_id = 1\nsso_role_name = Dev\n[sso-session corp]\nsso_region = us-east-1\nsso_start_url = https://corp/start\n[profile login]\nlogin_session = arn:aws:iam::1:user/me\n";
    let fixture = fixture(config, "");
    let sso_dir = fixture.dir.path().join(".aws/sso/cache");
    std::fs::create_dir_all(&sso_dir).unwrap();
    let key = hex(digest::digest(&digest::SHA1_FOR_LEGACY_USE_ONLY, b"corp").as_ref());
    let expired = json!({"accessToken": "t", "expiresAt": "2001-01-01T00:00:00Z"});
    std::fs::write(sso_dir.join(format!("{key}.json")), expired.to_string()).unwrap();
    let error = resolve(&input(&fixture.env, Some("sso"))).await.unwrap_err();
    assert!(error.contains("aws sso login --profile sso"), "{error}");

    let login_dir = fixture.dir.path().join(".aws/login/cache");
    std::fs::create_dir_all(&login_dir).unwrap();
    let key = hex(digest::digest(&digest::SHA256, b"arn:aws:iam::1:user/me").as_ref());
    let token = json!({"accessToken": {"accessKeyId": "ASIALOGIN", "secretAccessKey": "s", "sessionToken": "t", "expiresAt": "2099-01-01T00:00:00Z"}});
    std::fs::write(login_dir.join(format!("{key}.json")), token.to_string()).unwrap();
    let credentials = resolve(&input(&fixture.env, Some("login"))).await.unwrap();
    assert_eq!(credentials.access_key_id, "ASIALOGIN");
}
