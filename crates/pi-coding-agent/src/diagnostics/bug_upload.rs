//! `/bug` upload: the report files as multipart form data to the Radius gateway, anonymous or
//! with the user's Radius token (upstream `core/bug-report-upload.ts`, `core/radius.ts`).

use pi_ai::auth::oauth::{DEFAULT_RADIUS_GATEWAY, normalize_radius_gateway_url};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::bug_report::{BugReportBundle, bug_report_files};

pub const ENV_RADIUS_GATEWAY: &str = "PI_RADIUS_GATEWAY";

/// The Radius gateway origin, honoring `PI_RADIUS_GATEWAY`.
pub fn radius_gateway_url() -> String {
    let configured = pi_ai::env::process_env_value(ENV_RADIUS_GATEWAY).filter(|value| !value.is_empty());
    normalize_radius_gateway_url(configured.as_deref().unwrap_or(DEFAULT_RADIUS_GATEWAY))
}

/// A multipart body with one file part per report file, and its content type.
pub fn multipart_body(bundle: &BugReportBundle, boundary: &str) -> (Vec<u8>, String) {
    let mut body = Vec::new();
    for file in bug_report_files(bundle) {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{name}\"\r\nContent-Type: {}\r\n\r\n",
                file.content_type,
                name = file.name
            )
            .as_bytes(),
        );
        body.extend_from_slice(file.data.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (body, format!("multipart/form-data; boundary={boundary}"))
}

#[derive(Clone, Debug, Default)]
pub struct UploadBugReportOptions {
    /// Radius token; anonymous without it.
    pub token: Option<String>,
    /// The gateway origin; [`radius_gateway_url`] by default.
    pub gateway_url: Option<String>,
    pub client: Option<reqwest::Client>,
}

/// Upload the report and return its id.
pub async fn upload_bug_report(
    bundle: &BugReportBundle,
    options: &UploadBugReportOptions,
    cancel: &CancellationToken,
) -> Result<String, String> {
    let gateway = options.gateway_url.clone().unwrap_or_else(radius_gateway_url);
    let url = reqwest::Url::parse(&gateway)
        .and_then(|base| base.join("/v1/bug-reports"))
        .map_err(|error| format!("Bug report upload failed: {error}"))?;
    let boundary = format!("pi-bug-report-{}", crate::session::ids::uuid_v4().replace('-', ""));
    let (body, content_type) = multipart_body(bundle, &boundary);
    let client = options.client.clone().unwrap_or_else(pi_ai::http::shared_client);
    let mut request = client.post(url).header("Content-Type", content_type).body(body);
    if let Some(token) = options.token.as_deref().filter(|token| !token.is_empty()) {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let exchange = async {
        let response = request.send().await.map_err(|error| format!("Bug report upload failed: {error}"))?;
        let status = response.status();
        let json: Option<Value> = response.bytes().await.ok().and_then(|body| serde_json::from_slice(&body).ok());
        let ok = json.as_ref().and_then(|json| json.get("ok")).and_then(Value::as_bool) == Some(true);
        let id = json.as_ref().and_then(|json| json.pointer("/bug_report/id")).and_then(Value::as_str);
        if status.is_success()
            && ok
            && let Some(id) = id
        {
            return Ok(id.to_string());
        }
        let field =
            |key: &str| json.as_ref().and_then(|json| json.get(key)).and_then(Value::as_str).filter(|v| !v.is_empty());
        let detail = if ok { None } else { field("description").or_else(|| field("error")) };
        let fallback = status.canonical_reason().map(String::from).unwrap_or_else(|| status.as_u16().to_string());
        Err(format!("Bug report upload failed: {}", detail.map(String::from).unwrap_or(fallback)))
    };
    tokio::select! {
        _ = cancel.cancelled() => Err("Bug report upload cancelled".into()),
        result = exchange => result,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::platform::test_http::{TestReply, TestServer, client};

    fn bundle() -> BugReportBundle {
        BugReportBundle {
            metadata: json!({"id": "r1"}),
            diagnostics: json!({"assistant": []}),
            session_jsonl: Some("{\"type\":\"session\"}\n".into()),
            summary: Some("## What went wrong".into()),
        }
    }

    fn options(server: &TestServer, token: Option<&str>) -> UploadBugReportOptions {
        UploadBugReportOptions {
            token: token.map(String::from),
            gateway_url: Some(format!("{}/ignored-path", server.url)),
            client: Some(client()),
        }
    }

    #[tokio::test]
    async fn uploads_multipart_files_with_the_radius_token() {
        let server = TestServer::start(|request| {
            assert_eq!(request.path(), "/v1/bug-reports");
            TestReply::json(&json!({"ok": true, "bug_report": {"id": "bug-42"}}))
        })
        .await;
        let id = upload_bug_report(&bundle(), &options(&server, Some("tok")), &CancellationToken::new()).await.unwrap();
        assert_eq!(id, "bug-42");
        let request = &server.requests()[0];
        assert_eq!(request.header("authorization"), Some("Bearer tok"));
        let content_type = request.header("content-type").unwrap();
        let boundary = content_type.strip_prefix("multipart/form-data; boundary=").unwrap();
        let body = String::from_utf8(request.body.clone()).unwrap();
        for name in ["report.json", "diagnostics.json", "session.jsonl", "summary.md"] {
            assert!(body.contains(&format!("name=\"{name}\"; filename=\"{name}\"")), "{name}");
        }
        assert!(body.contains("Content-Type: application/x-ndjson\r\n\r\n{\"type\":\"session\"}\n\r\n"));
        assert!(body.contains("## What went wrong\n\r\n"));
        assert!(body.ends_with(&format!("--{boundary}--\r\n")));
    }

    #[tokio::test]
    async fn reports_gateway_errors_and_stays_anonymous_without_a_token() {
        let server = TestServer::start(|request| match request.header("x-case") {
            _ if request.header("authorization").is_some() => TestReply::status(500, "unexpected"),
            _ => TestReply::bytes(
                413,
                br#"{"ok": false, "error": "too_large", "description": "Report too large"}"#.to_vec(),
            ),
        })
        .await;
        let error = upload_bug_report(&bundle(), &options(&server, None), &CancellationToken::new()).await.unwrap_err();
        assert_eq!(error, "Bug report upload failed: Report too large");

        let plain = TestServer::start(|_| TestReply::status(500, "oops")).await;
        let error =
            upload_bug_report(&bundle(), &options(&plain, Some("")), &CancellationToken::new()).await.unwrap_err();
        assert_eq!(error, "Bug report upload failed: Internal Server Error");
        let missing_id = TestServer::start(|_| TestReply::json(&json!({"ok": true}))).await;
        let error =
            upload_bug_report(&bundle(), &options(&missing_id, None), &CancellationToken::new()).await.unwrap_err();
        assert_eq!(error, "Bug report upload failed: OK");
    }
}
