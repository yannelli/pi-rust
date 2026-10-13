use std::time::{Duration, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use super::aws::signed_request_token;
use super::tests::{ID_TOKEN_TYPE, WORKLOAD_AUDIENCE, env, sts_reply, token};
use crate::providers::aws::sigv4::Credentials;
use crate::providers::mock_http::{MockHttp, Reply};

const AWS_TOKEN_TYPE: &str = "urn:ietf:params:aws:token-type:aws4_request";

fn credentials(server: &MockHttp, source: Value, subject_token_type: &str) -> Value {
    json!({
        "type": "external_account", "audience": WORKLOAD_AUDIENCE, "subject_token_type": subject_token_type,
        "token_url": format!("{}/v1/token", server.base_url), "credential_source": source
    })
}

#[tokio::test]
async fn url_sources_send_headers_and_read_text_or_json() {
    let server = MockHttp::start(vec![
        Reply::text(200, "text/plain", "url-subject"),
        sts_reply("sts-url"),
        Reply::json(200, &json!({"value": "json-url-subject"})),
        sts_reply("sts-url-json"),
    ])
    .await;
    let source = json!({"url": format!("{}/subject", server.base_url), "headers": {"Metadata": "True"}});
    let mut config = credentials(&server, source, ID_TOKEN_TYPE);
    let (access, _) = token(&config, &env(&[])).await.unwrap();
    assert_eq!(access.token, "sts-url");
    let subject = server.request(0);
    assert_eq!((subject.method.as_str(), subject.path.as_str()), ("GET", "/subject"));
    assert_eq!(subject.headers["metadata"], "True");
    assert_eq!(server.request(1).form()["subject_token"], "url-subject");
    assert!(server.request(1).headers["x-goog-api-client"].contains("source/url"));

    config["credential_source"]["format"] = json!({"type": "json", "subject_token_field_name": "value"});
    token(&config, &env(&[])).await.unwrap();
    assert_eq!(server.request(3).form()["subject_token"], "json-url-subject");

    config["credential_source"]["file"] = json!("/tmp/both");
    let error = token(&config, &env(&[])).await.unwrap_err();
    assert!(error.starts_with("No valid Identity Pool \"credential_source\" provided"), "{error}");
}

#[cfg(unix)]
#[tokio::test]
async fn executable_sources_require_opt_in_and_pass_context() {
    let server = MockHttp::start(vec![sts_reply("sts-exec")]).await;
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("token.sh");
    let response = r#"{"version":1,"success":true,"token_type":"urn:ietf:params:oauth:token-type:id_token","id_token":"exec-%s-%s-%s","expiration_time":9999999999}"#;
    let body = format!(
        "printf '{response}' \"$GOOGLE_EXTERNAL_ACCOUNT_INTERACTIVE\" \"$GOOGLE_EXTERNAL_ACCOUNT_TOKEN_TYPE\" \"$GOOGLE_EXTERNAL_ACCOUNT_IMPERSONATED_EMAIL\"\n"
    );
    std::fs::write(&script, body).unwrap();
    let source = json!({"executable": {"command": format!("sh \"{}\"", script.display()), "timeout_millis": 5000}});
    let mut config = credentials(&server, source, ID_TOKEN_TYPE);
    let error = token(&config, &env(&[("GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES", "0")])).await.unwrap_err();
    assert!(error.starts_with("Pluggable Auth executables need to be explicitly allowed"), "{error}");
    let allowed = env(&[("GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES", "1")]);
    let (access, _) = token(&config, &allowed).await.unwrap();
    assert_eq!(access.token, "sts-exec");
    let form = server.request(0).form();
    assert_eq!(form["subject_token"], format!("exec-0-{ID_TOKEN_TYPE}-"));
    assert!(server.request(0).headers["x-goog-api-client"].contains("source/executable"));

    std::fs::write(&script, "printf '{\"version\":1,\"success\":false,\"code\":\"401\",\"message\":\"denied\"}'\n")
        .unwrap();
    let error = token(&config, &allowed).await.unwrap_err();
    assert_eq!(error, "The executable failed with exit code: 401 and error message: denied.");
    std::fs::write(&script, "echo broken; exit 3\n").unwrap();
    let error = token(&config, &allowed).await.unwrap_err();
    assert_eq!(error, "The executable failed with exit code: 3 and error message: broken\n.");
    config["credential_source"]["executable"]["timeout_millis"] = json!(1000);
    let error = token(&config, &allowed).await.unwrap_err();
    assert_eq!(error, "Timeout must be between 5000 and 120000 milliseconds.");
}

#[cfg(unix)]
#[tokio::test]
async fn executable_output_files_are_reused_while_unexpired() {
    let server = MockHttp::start(vec![sts_reply("sts-cached")]).await;
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.json");
    let cached = json!({"version": 1, "success": true, "token_type": "urn:ietf:params:oauth:token-type:jwt",
        "id_token": "cached-subject", "expiration_time": 9_999_999_999i64});
    std::fs::write(&output, cached.to_string()).unwrap();
    let source = json!({"executable": {"command": "false", "output_file": output.display().to_string()}});
    let config = credentials(&server, source, "urn:ietf:params:oauth:token-type:jwt");
    token(&config, &env(&[("GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES", "1")])).await.unwrap();
    assert_eq!(server.request(0).form()["subject_token"], "cached-subject");
}

#[test]
fn aws_subject_tokens_are_signed_get_caller_identity_requests() {
    let mut credentials = Credentials::new("AKIDEXAMPLE", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY");
    credentials.session_token = Some("session".into());
    let url = "https://sts.{region}.amazonaws.com?Action=GetCallerIdentity&Version=2011-06-15";
    let time = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let token = signed_request_token(url, "us-east-2", &credentials, WORKLOAD_AUDIENCE, time).unwrap();
    let decoded = percent_decode(&token);
    let parsed: Value = serde_json::from_str(&decoded).unwrap();
    assert_eq!(parsed["url"], "https://sts.us-east-2.amazonaws.com?Action=GetCallerIdentity&Version=2011-06-15");
    assert_eq!(parsed["method"], "POST");
    let keys: Vec<&str> = parsed["headers"].as_array().unwrap().iter().map(|h| h["key"].as_str().unwrap()).collect();
    assert_eq!(keys, ["authorization", "host", "x-amz-date", "x-amz-security-token", "x-goog-cloud-target-resource"]);
    let value = |index: usize| parsed["headers"][index]["value"].as_str().unwrap().to_string();
    assert!(value(0).starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20231114/us-east-2/sts/aws4_request, "));
    assert!(value(0).contains("SignedHeaders=host;x-amz-date;x-amz-security-token, Signature="));
    assert_eq!(
        (value(1), value(2), value(3)),
        ("sts.us-east-2.amazonaws.com".into(), "20231114T221320Z".into(), "session".into())
    );
    assert_eq!(value(4), WORKLOAD_AUDIENCE);
    assert!(!token.contains('"') && !token.contains(':'), "the token is URI encoded");
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            out.push(u8::from_str_radix(&text[index + 1..index + 3], 16).unwrap());
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

#[tokio::test]
async fn aws_sources_read_environment_credentials() {
    let server = MockHttp::start(vec![sts_reply("sts-aws")]).await;
    let source = json!({"environment_id": "aws1", "region_url": "http://127.0.0.1:9/region",
        "url": "http://127.0.0.1:9/role", "regional_cred_verification_url": "https://sts.{region}.amazonaws.com?Action=GetCallerIdentity&Version=2011-06-15"});
    let mut config = credentials(&server, source, AWS_TOKEN_TYPE);
    let aws_env =
        env(&[("AWS_REGION", "eu-west-1"), ("AWS_ACCESS_KEY_ID", "AKID"), ("AWS_SECRET_ACCESS_KEY", "secret")]);
    let (access, _) = token(&config, &aws_env).await.unwrap();
    assert_eq!(access.token, "sts-aws");
    let request = server.request(0);
    assert!(request.headers["x-goog-api-client"].contains("source/aws"));
    let form = request.form();
    assert_eq!(form["subject_token_type"], AWS_TOKEN_TYPE);
    let subject: Value = serde_json::from_str(&percent_decode(&form["subject_token"])).unwrap();
    assert_eq!(subject["url"], "https://sts.eu-west-1.amazonaws.com?Action=GetCallerIdentity&Version=2011-06-15");
    assert!(subject["headers"][0]["value"].as_str().unwrap().contains("Credential=AKID/"));

    config["credential_source"]["environment_id"] = json!("aws2");
    assert_eq!(token(&config, &aws_env).await.unwrap_err(), "aws version \"2\" is not supported in the current build.");
}

#[tokio::test]
async fn aws_sources_fall_back_to_the_instance_metadata_service() {
    let aws_vars = ["AWS_REGION", "AWS_DEFAULT_REGION", "AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"];
    if aws_vars.iter().any(|name| std::env::var(name).is_ok_and(|value| !value.is_empty())) {
        return;
    }
    let role = json!({"AccessKeyId": "ASIAMETA", "SecretAccessKey": "meta-secret", "Token": "meta-session"});
    let server = MockHttp::start(vec![
        Reply::text(200, "text/plain", "imds-token"),
        Reply::text(200, "text/plain", "us-west-2b"),
        Reply::text(200, "text/plain", "imds-token"),
        Reply::text(200, "text/plain", "role-name"),
        Reply::json(200, &role),
        sts_reply("sts-imds"),
    ])
    .await;
    let source = json!({"environment_id": "aws1", "region_url": format!("{}/region", server.base_url),
        "url": format!("{}/credentials", server.base_url), "imdsv2_session_token_url": format!("{}/token", server.base_url),
        "regional_cred_verification_url": "https://sts.{region}.amazonaws.com?Action=GetCallerIdentity&Version=2011-06-15"});
    let config = credentials(&server, source, AWS_TOKEN_TYPE);
    token(&config, &env(&[])).await.unwrap();
    let session = server.request(0);
    assert_eq!((session.method.as_str(), session.path.as_str()), ("PUT", "/token"));
    assert_eq!(session.headers["x-aws-ec2-metadata-token-ttl-seconds"], "300");
    assert_eq!(server.request(1).headers["x-aws-ec2-metadata-token"], "imds-token");
    assert_eq!(server.request(4).path, "/credentials/role-name");
    let subject: Value = serde_json::from_str(&percent_decode(&server.request(5).form()["subject_token"])).unwrap();
    assert_eq!(subject["url"], "https://sts.us-west-2.amazonaws.com?Action=GetCallerIdentity&Version=2011-06-15");
    let headers = subject["headers"].as_array().unwrap();
    assert!(headers[0]["value"].as_str().unwrap().contains("Credential=ASIAMETA/"));
    assert_eq!(headers[3], json!({"key": "x-amz-security-token", "value": "meta-session"}));
}

#[tokio::test]
async fn certificate_sources_send_the_certificate_chain() {
    let server = MockHttp::start(vec![sts_reply("sts-mtls"), sts_reply("sts-chain")]).await;
    let dir = tempfile::tempdir().unwrap();
    let cert_pem = include_str!("test_cert.pem");
    let key_pem = format!("-----BEGIN PRIVATE KEY-----\n{}-----END PRIVATE KEY-----\n", include_str!("test_key.b64"));
    let (cert_path, key_path) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
    std::fs::write(&cert_path, cert_pem).unwrap();
    std::fs::write(&key_path, key_pem).unwrap();
    let workload = json!({"cert_configs": {"workload": {"cert_path": cert_path, "key_path": key_path}}});
    let config_path = dir.path().join("certificate_config.json");
    std::fs::write(&config_path, workload.to_string()).unwrap();
    let source = json!({"certificate": {"certificate_config_location": config_path.display().to_string()}});
    let mut config = credentials(&server, source, "urn:ietf:params:oauth:token-type:mtls");
    let (access, _) = token(&config, &env(&[])).await.unwrap();
    assert_eq!(access.token, "sts-mtls");
    let body: String = cert_pem.lines().filter(|line| !line.starts_with("-----")).collect();
    let leaf = STANDARD.encode(STANDARD.decode(body).unwrap());
    let form = server.request(0).form();
    assert_eq!(serde_json::from_str::<Value>(&form["subject_token"]).unwrap(), json!([leaf]));
    assert!(server.request(0).headers["x-goog-api-client"].contains("source/certificate"));

    let chain_path = dir.path().join("chain.pem");
    std::fs::write(&chain_path, cert_pem).unwrap();
    config["credential_source"]["certificate"]["trust_chain_path"] = json!(chain_path);
    token(&config, &env(&[])).await.unwrap();
    assert_eq!(serde_json::from_str::<Value>(&server.request(1).form()["subject_token"]).unwrap(), json!([leaf]));

    config["credential_source"]["certificate"]["use_default_certificate_config"] = json!(true);
    let error = token(&config, &env(&[])).await.unwrap_err();
    assert_eq!(error, "Both `useDefaultCertificateConfig` and `certificateConfigLocation` cannot be provided.");
}
