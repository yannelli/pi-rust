use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::export::ExportTheme;
use crate::export::share::{
    CommandOutput, RadiusTarget, ShareCommandRunner, ShareError, ShareOutcome, ShareRequest, check_gh_auth,
    create_gist, share_session, share_viewer_url, share_viewer_url_with, upload_radius_artifact,
};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/export_session.jsonl")
}

type Respond = dyn Fn(&[String]) -> std::io::Result<CommandOutput> + Send + Sync;

struct MockRunner {
    calls: Mutex<Vec<Vec<String>>>,
    respond: Box<Respond>,
    delay_gist: bool,
}

impl MockRunner {
    fn new(respond: impl Fn(&[String]) -> std::io::Result<CommandOutput> + Send + Sync + 'static) -> Self {
        Self { calls: Mutex::new(Vec::new()), respond: Box::new(respond), delay_gist: false }
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl ShareCommandRunner for MockRunner {
    async fn run(&self, program: &str, args: &[String]) -> std::io::Result<CommandOutput> {
        assert_eq!(program, "gh");
        self.calls.lock().unwrap().push(args.to_vec());
        if self.delay_gist && args[0] == "gist" {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
        (self.respond)(args)
    }
}

fn output(status: i32, stdout: &str, stderr: &str) -> std::io::Result<CommandOutput> {
    Ok(CommandOutput { status: Some(status), stdout: stdout.into(), stderr: stderr.into() })
}

fn request<'a>(
    session_file: &'a Path,
    runner: &'a MockRunner,
    radius: Option<RadiusTarget>,
    cancel: CancellationToken,
) -> ShareRequest<'a> {
    ShareRequest {
        session_file,
        system_prompt: "sys".into(),
        tools: Vec::new(),
        theme: ExportTheme::dark(),
        tool_renderer: None,
        radius,
        runner,
        http: reqwest::Client::builder().no_proxy().build().unwrap(),
        cancel,
    }
}

#[test]
fn viewer_url_appends_the_gist_id() {
    assert_eq!(share_viewer_url_with(Some("https://example.com/view/"), "abc"), "https://example.com/view/#abc");
    assert_eq!(share_viewer_url_with(None, "abc"), "https://pi.dev/session/#abc");
    assert_eq!(share_viewer_url_with(Some(""), "abc"), "https://pi.dev/session/#abc");
}

#[tokio::test]
async fn gist_share_uploads_the_html_export() {
    let uploaded = std::sync::Arc::new(Mutex::new(None::<(PathBuf, String)>));
    let seen = uploaded.clone();
    let runner = MockRunner::new(move |args| match args[0].as_str() {
        "auth" => output(0, "", ""),
        _ => {
            let path = PathBuf::from(args.last().unwrap());
            *seen.lock().unwrap() = Some((path.clone(), std::fs::read_to_string(&path).unwrap()));
            output(0, "https://gist.github.com/user/abc123\n", "")
        }
    });
    let outcome = share_session(request(&fixture(), &runner, None, CancellationToken::new())).await.unwrap();
    let preview_url = share_viewer_url("abc123");
    assert_eq!(outcome, ShareOutcome::Gist { gist_url: "https://gist.github.com/user/abc123".into(), preview_url });
    assert!(outcome.status_message().starts_with("Share URL: "));

    let calls = runner.calls();
    assert_eq!(calls[0], ["auth", "status"]);
    assert_eq!(&calls[1][..3], ["gist", "create", "--public=false"]);
    let (path, html) = uploaded.lock().unwrap().clone().unwrap();
    assert!(path.ends_with("session.html"));
    assert!(html.contains("<title>Session Export</title>"));
    assert!(!path.parent().unwrap().exists(), "the temporary directory is removed");
}

#[tokio::test]
async fn gh_failures_use_upstream_messages() {
    fn failed<T>(message: &str) -> Result<T, ShareError> {
        Err(ShareError::Failed(message.into()))
    }
    let runner = MockRunner::new(|_| output(1, "", "not logged in"));
    assert_eq!(check_gh_auth(&runner).await, failed("GitHub CLI is not logged in. Run 'gh auth login' first."));
    let runner = MockRunner::new(|_| Err(std::io::Error::from(std::io::ErrorKind::NotFound)));
    assert_eq!(
        check_gh_auth(&runner).await,
        failed("GitHub CLI (gh) is not installed. Install it from https://cli.github.com/")
    );
    let file = Path::new("session.html");
    let runner = MockRunner::new(|_| output(1, "", "  boom \n"));
    assert_eq!(create_gist(&runner, file).await, failed("Failed to create gist: boom"));
    let runner = MockRunner::new(|_| output(2, "", ""));
    assert_eq!(create_gist(&runner, file).await, failed("Failed to create gist: Unknown error"));
    let runner = MockRunner::new(|_| output(0, "https://gist.github.com/user/\n", ""));
    assert_eq!(create_gist(&runner, file).await, failed("Failed to parse gist ID from gh output"));
}

#[tokio::test]
async fn cancelling_stops_the_gist_upload() {
    let mut runner = MockRunner::new(|_| output(0, "https://gist.github.com/u/x", ""));
    runner.delay_gist = true;
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        trigger.cancel();
    });
    assert_eq!(share_session(request(&fixture(), &runner, None, cancel)).await, Err(ShareError::Cancelled));
}

/// Serve one HTTP response; the task returns the raw request.
async fn serve_once(status: &'static str, body: &'static str) -> (String, tokio::task::JoinHandle<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            let read = socket.read(&mut buffer).await.unwrap();
            request.extend_from_slice(&buffer[..read]);
            let text = String::from_utf8_lossy(&request).to_string();
            if let Some(head_end) = text.find("\r\n\r\n") {
                let length = text[..head_end]
                    .lines()
                    .find_map(|line| line.to_lowercase().strip_prefix("content-length: ").map(str::to_string))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if request.len() >= head_end + 4 + length || read == 0 {
                    break;
                }
            }
        }
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        String::from_utf8_lossy(&request).to_string()
    });
    (url, task)
}

#[tokio::test]
async fn radius_share_posts_the_branch_jsonl() {
    let (url, server) =
        serve_once("201 Created", r#"{"artifact":{"canonical_url":"https://radius.example/a/1"}}"#).await;
    let runner = MockRunner::new(|_| panic!("gh must not run when Radius is available"));
    let radius = RadiusTarget { gateway: format!("{url}/ignored/path"), token: "tok".into() };
    let outcome = share_session(request(&fixture(), &runner, Some(radius), CancellationToken::new())).await.unwrap();
    assert_eq!(outcome, ShareOutcome::Radius { url: "https://radius.example/a/1".into() });
    assert_eq!(outcome.status_message(), "Share URL: https://radius.example/a/1");

    let raw = server.await.unwrap();
    let lower = raw.to_lowercase();
    assert!(raw.starts_with("POST /v1/artifacts?visibility=organization&title=Pi+session HTTP/1.1\r\n"), "{raw}");
    assert!(lower.contains("authorization: bearer tok\r\n"));
    assert!(lower.contains("content-type: application/x-ndjson\r\n"));
    let body = &raw[raw.find("\r\n\r\n").unwrap() + 4..];
    let last: serde_json::Value = serde_json::from_str(body.trim_end().lines().last().unwrap()).unwrap();
    assert_eq!(last["customType"], "pi.share");
    assert!(runner.calls().is_empty());
}

#[tokio::test]
async fn radius_errors_report_the_reason() {
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let (url, _server) = serve_once("403 Forbidden", r#"{"error":"organization required"}"#).await;
    let error = upload_radius_artifact(&client, &url, "tok", b"{}\n".to_vec()).await.unwrap_err();
    assert_eq!(error.to_string(), "Failed to upload Radius artifact: organization required");
    let (url, _server) = serve_once("500 Internal Server Error", "oops").await;
    let error = upload_radius_artifact(&client, &url, "tok", b"{}\n".to_vec()).await.unwrap_err();
    assert_eq!(error.to_string(), "Failed to upload Radius artifact: Internal Server Error");
}
