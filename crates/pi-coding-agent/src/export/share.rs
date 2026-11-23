//! `/share` (upstream `modes/interactive/session-share.ts`): upload the HTML export to the user's
//! pi-rust.com account (pi-rust extension), upload the branch JSONL as a Radius artifact, or
//! publish the HTML export as a private GitHub gist through the `gh` CLI and link it in the share
//! viewer. Gist links open on pi-rust.com unless `sessionShareViewer` is `pi.dev`.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::html::{ExportTool, HtmlExportOptions, export_session_html};
use super::jsonl::export_session_to_jsonl;
use super::pi_rust_share::{PiRustAccount, PiRustError, upload_share};
use super::session_file::random_hex;
use super::theme::ExportTheme;
use super::tool_renderer::ToolHtmlRenderer;
use crate::settings::schema::{SessionShareDestination, SessionShareViewer};

/// Upstream's gist viewer, the default.
pub const PI_DEV_SHARE_VIEWER_URL: &str = "https://pi.dev/session/";
/// pi-rust.com's gist viewer (pi-rust extension), for `sessionShareViewer: "pi-rust"`.
pub const PI_RUST_SHARE_VIEWER_URL: &str = "https://pi-rust.com/session/";
pub const DEFAULT_RADIUS_GATEWAY: &str = "https://radius.pi.dev";
pub const ENV_SHARE_VIEWER_URL: &str = "PI_SHARE_VIEWER_URL";
/// The share title when the session has no name.
pub const DEFAULT_SHARE_TITLE: &str = "Pi session";

/// The viewer base URL of a `sessionShareViewer` value.
pub fn viewer_base_url(viewer: SessionShareViewer) -> &'static str {
    match viewer {
        SessionShareViewer::PiRust => PI_RUST_SHARE_VIEWER_URL,
        SessionShareViewer::PiDev => PI_DEV_SHARE_VIEWER_URL,
    }
}

/// Viewer link for a gist: `$PI_SHARE_VIEWER_URL` (or the `viewer` setting's base) followed by `#<gist id>`.
pub fn share_viewer_url(viewer: SessionShareViewer, gist_id: &str) -> String {
    share_viewer_url_with(std::env::var(ENV_SHARE_VIEWER_URL).ok().as_deref(), viewer, gist_id)
}

/// [`share_viewer_url`] with an explicit override; `None` or empty uses the `viewer` setting's base.
pub fn share_viewer_url_with(base: Option<&str>, viewer: SessionShareViewer, gist_id: &str) -> String {
    let base = base.filter(|url| !url.is_empty()).unwrap_or(viewer_base_url(viewer));
    format!("{base}#{gist_id}")
}

/// Where one `/share` goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareRoute {
    /// The signed-in pi-rust.com account.
    PiRust,
    /// The pi-rust.com account after the user signs in.
    PiRustSignIn,
    Radius,
    Gist,
}

pub const RADIUS_NOT_CONFIGURED: &str = "Radius authentication is not configured. Run '/login radius' first.";

/// Whether `/share` looks for a pi-rust.com account: always for `pi-rust`, and for `auto` unless
/// the user chose pi.dev's viewer, so `auto` then behaves as upstream (Radius, else a gist).
pub fn uses_pi_rust_account(destination: SessionShareDestination, viewer: SessionShareViewer) -> bool {
    match destination {
        SessionShareDestination::PiRust => true,
        SessionShareDestination::Auto => viewer == SessionShareViewer::PiRust,
        SessionShareDestination::Gist | SessionShareDestination::Radius => false,
    }
}

/// Whether choosing a route for `destination` needs to know about a Radius credential.
pub fn route_needs_radius(destination: SessionShareDestination, pi_rust_signed_in: bool) -> bool {
    match destination {
        SessionShareDestination::Auto => !pi_rust_signed_in,
        SessionShareDestination::Radius => true,
        SessionShareDestination::Gist | SessionShareDestination::PiRust => false,
    }
}

/// The route for `destination`: `auto` prefers the signed-in pi-rust.com account, then Radius,
/// then a gist. An explicit `radius` without a credential fails.
pub fn share_route(
    destination: SessionShareDestination,
    pi_rust_signed_in: bool,
    radius_available: bool,
) -> Result<ShareRoute, ShareError> {
    Ok(match destination {
        SessionShareDestination::PiRust if pi_rust_signed_in => ShareRoute::PiRust,
        SessionShareDestination::PiRust => ShareRoute::PiRustSignIn,
        SessionShareDestination::Auto if pi_rust_signed_in => ShareRoute::PiRust,
        SessionShareDestination::Auto | SessionShareDestination::Radius if radius_available => ShareRoute::Radius,
        SessionShareDestination::Radius => return Err(ShareError::Failed(RADIUS_NOT_CONFIGURED.into())),
        SessionShareDestination::Auto | SessionShareDestination::Gist => ShareRoute::Gist,
    })
}

/// The trailing `pi.share` custom entry with the system prompt and tool schemas.
pub fn create_share_trailing_entries(
    system_prompt: &str,
    tools: &[ExportTool],
    parent_id: Option<&str>,
    timestamp: &str,
) -> Vec<Value> {
    let tools: Vec<Value> = tools
        .iter()
        .map(|tool| {
            let mut map = Map::new();
            map.insert("name".into(), tool.name.clone().into());
            if let Some(description) = &tool.description {
                map.insert("description".into(), description.clone().into());
            }
            map.insert("parameters".into(), tool.parameters.clone());
            Value::Object(map)
        })
        .collect();
    vec![json!({
        "type": "custom",
        "customType": "pi.share",
        "id": random_hex(8),
        "parentId": parent_id,
        "timestamp": timestamp,
        "data": { "systemPrompt": system_prompt, "tools": tools },
    })]
}

/// Export the current branch plus the `pi.share` entry to `file_path`.
pub fn export_session_for_share(
    session_file: &Path,
    file_path: &Path,
    system_prompt: &str,
    tools: &[ExportTool],
) -> anyhow::Result<PathBuf> {
    let trailing =
        |parent: Option<&str>, timestamp: &str| create_share_trailing_entries(system_prompt, tools, parent, timestamp);
    let output = file_path.to_string_lossy();
    let cwd = std::env::current_dir()?;
    export_session_to_jsonl(session_file, Some(&output), &cwd, Some(&trailing))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShareError {
    /// The user aborted the upload or gist creation.
    Cancelled,
    /// A user-facing error message, as upstream shows it.
    Failed(String),
}

impl std::fmt::Display for ShareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("Share cancelled"),
            Self::Failed(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ShareError {}

/// `PiRust` is a share in the user's pi-rust.com account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShareOutcome {
    Radius { url: String },
    Gist { gist_url: String, preview_url: String },
    PiRust { url: String },
}

impl ShareOutcome {
    /// The status line upstream shows (without terminal hyperlinks).
    pub fn status_message(&self) -> String {
        match self {
            Self::Radius { url } | Self::PiRust { url } => format!("Share URL: {url}"),
            Self::Gist { gist_url, preview_url } => format!("Share URL: {preview_url}\nGist: {gist_url}"),
        }
    }
}

/// Exit status (`None` when killed by a signal) and captured output of a command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandOutput {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Runs the `gh` CLI. Tests substitute a mock.
#[async_trait]
pub trait ShareCommandRunner: Send + Sync {
    async fn run(&self, program: &str, args: &[String]) -> std::io::Result<CommandOutput>;
}

/// Spawns real processes; dropping the future (on cancel) kills the child.
pub struct SystemCommandRunner;

#[async_trait]
impl ShareCommandRunner for SystemCommandRunner {
    async fn run(&self, program: &str, args: &[String]) -> std::io::Result<CommandOutput> {
        let output = tokio::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output()
            .await?;
        Ok(CommandOutput {
            status: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Fails unless `gh auth status` succeeds.
pub async fn check_gh_auth(runner: &dyn ShareCommandRunner) -> Result<(), ShareError> {
    match runner.run("gh", &["auth".into(), "status".into()]).await {
        Ok(output) if output.status == Some(0) => Ok(()),
        Ok(_) => Err(ShareError::Failed("GitHub CLI is not logged in. Run 'gh auth login' first.".into())),
        Err(_) => {
            Err(ShareError::Failed("GitHub CLI (gh) is not installed. Install it from https://cli.github.com/".into()))
        }
    }
}

/// `gh gist create --public=false <file>`, returning the gist and viewer URLs.
pub async fn create_gist(
    runner: &dyn ShareCommandRunner,
    file: &Path,
    viewer: SessionShareViewer,
) -> Result<ShareOutcome, ShareError> {
    let args = ["gist".into(), "create".into(), "--public=false".into(), file.to_string_lossy().into_owned()];
    let output =
        runner.run("gh", &args).await.map_err(|error| ShareError::Failed(format!("Failed to create gist: {error}")))?;
    if output.status != Some(0) {
        let stderr = output.stderr.trim();
        let reason = if stderr.is_empty() { "Unknown error" } else { stderr };
        return Err(ShareError::Failed(format!("Failed to create gist: {reason}")));
    }
    let gist_url = output.stdout.trim().to_string();
    let gist_id = gist_url.rsplit('/').next().unwrap_or_default();
    if gist_id.is_empty() {
        return Err(ShareError::Failed("Failed to parse gist ID from gh output".into()));
    }
    let preview_url = share_viewer_url(viewer, gist_id);
    Ok(ShareOutcome::Gist { gist_url, preview_url })
}

/// Upload a share JSONL as an organization-visible Radius artifact; returns its canonical URL.
pub async fn upload_radius_artifact(
    client: &reqwest::Client,
    gateway: &str,
    token: &str,
    body: Vec<u8>,
) -> Result<String, ShareError> {
    let failed = |reason: String| ShareError::Failed(format!("Failed to upload Radius artifact: {reason}"));
    let mut url = reqwest::Url::parse(gateway)
        .and_then(|base| base.join("/v1/artifacts"))
        .map_err(|error| failed(error.to_string()))?;
    url.query_pairs_mut().append_pair("visibility", "organization").append_pair("title", "Pi session");
    let response = client
        .post(url)
        .bearer_auth(token)
        .header(reqwest::header::CONTENT_TYPE, "application/x-ndjson")
        .header(reqwest::header::CONTENT_LENGTH, body.len())
        .body(body)
        .send()
        .await
        .map_err(|error| failed(error.to_string()))?;
    let status = response.status();
    let json: Option<Value> = response.json().await.ok();
    let canonical_url = json.as_ref().and_then(|json| json.pointer("/artifact/canonical_url")?.as_str());
    match canonical_url {
        Some(url) if status.is_success() => Ok(url.to_string()),
        _ => {
            let error = json.as_ref().and_then(|json| json.get("error")?.as_str()).filter(|e| !e.is_empty());
            let reason = error.map(str::to_string).or_else(|| status.canonical_reason().map(str::to_string));
            Err(failed(reason.unwrap_or_else(|| status.as_u16().to_string())))
        }
    }
}

/// Radius gateway and bearer token for an authenticated `radius` provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RadiusTarget {
    pub gateway: String,
    pub token: String,
}

/// Everything [`share_session`] needs from the running session.
pub struct ShareRequest<'a> {
    pub session_file: &'a Path,
    pub system_prompt: String,
    pub tools: Vec<ExportTool>,
    pub theme: ExportTheme,
    pub tool_renderer: Option<&'a (dyn ToolHtmlRenderer + Sync)>,
    /// Set when the `radius` provider has a valid credential; the gist path is skipped then.
    pub radius: Option<RadiusTarget>,
    /// The signed-in pi-rust.com account; it wins over Radius and the gist.
    pub pi_rust: Option<PiRustAccount>,
    /// The share title on pi-rust.com.
    pub title: String,
    pub viewer: SessionShareViewer,
    pub runner: &'a dyn ShareCommandRunner,
    pub http: reqwest::Client,
    pub cancel: CancellationToken,
}

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Share the session to the pi-rust.com account, else through Radius, else as a private gist
/// (upstream `shareSession`).
pub async fn share_session(request: ShareRequest<'_>) -> Result<ShareOutcome, ShareError> {
    let dir = std::env::temp_dir().join(format!("pi-share-{}", random_hex(12)));
    crate::agent_session::output_file::create_private_dir(&dir).map_err(|error| export_failed(&error))?;
    let dir = TempDir(dir);
    let jsonl_file = dir.0.join("session.jsonl");
    let html_file = dir.0.join("session.html");

    export_session_for_share(request.session_file, &jsonl_file, &request.system_prompt, &request.tools)
        .map_err(|error| export_failed(&error))?;

    let cancel = request.cancel.clone();
    let work = async {
        if let Some(account) = &request.pi_rust {
            export_share_html(&request, &html_file)?;
            let html = std::fs::read(&html_file).map_err(|error| export_failed(&error))?;
            let share = upload_share(&request.http, account, &request.title, html, &cancel).await;
            return match share {
                Ok(share) => Ok(ShareOutcome::PiRust { url: share.url }),
                Err(PiRustError::Cancelled) => Err(ShareError::Cancelled),
                Err(error) => Err(ShareError::Failed(error.to_string())),
            };
        }
        if let Some(radius) = &request.radius {
            let body = std::fs::read(&jsonl_file).map_err(|error| export_failed(&error))?;
            let url = upload_radius_artifact(&request.http, &radius.gateway, &radius.token, body).await?;
            return Ok(ShareOutcome::Radius { url });
        }
        check_gh_auth(request.runner).await?;
        export_share_html(&request, &html_file)?;
        create_gist(request.runner, &html_file, request.viewer).await
    };
    tokio::select! {
        result = work => result,
        () = cancel.cancelled() => Err(ShareError::Cancelled),
    }
}

fn export_share_html(request: &ShareRequest<'_>, html_file: &Path) -> Result<(), ShareError> {
    let options = HtmlExportOptions {
        theme: request.theme.clone(),
        system_prompt: Some(request.system_prompt.clone()),
        tools: Some(request.tools.clone()),
        tool_renderer: request.tool_renderer.map(|renderer| renderer as &dyn ToolHtmlRenderer),
    };
    export_session_html(request.session_file, Some(&html_file.to_string_lossy()), &options)
        .map(drop)
        .map_err(|error| export_failed(&error))
}

fn export_failed(error: &dyn std::fmt::Display) -> ShareError {
    ShareError::Failed(format!("Failed to export session: {error}"))
}
