//! The pi-rust.com share service (pi-rust extension): sign-in with the OAuth 2.0 device
//! authorization grant (RFC 8628), the stored account token, and HTML share uploads.
//! `PI_SHARE_URL` replaces the `https://pi-rust.com` origin.

use std::path::{Path, PathBuf};

use pi_ai::auth::oauth::{DeviceCodePollOptions, DevicePoll, poll_device_code_flow};
use pi_ai::auth::{AuthError, AuthEvent, AuthInteraction};
use reqwest::{Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::platform::{VERSION, pi_user_agent};
use crate::settings::schema::{SessionShareExpiry, SessionShareVisibility};

pub const ENV_SHARE_URL: &str = "PI_SHARE_URL";
pub const DEFAULT_SHARE_URL: &str = "https://pi-rust.com";
/// The account file in the agent dir; separate from `auth.json`, which holds model providers.
pub const ACCOUNT_FILE: &str = "pi-rust-account.json";
/// The service name shown to users, for example in `/logout`.
pub const ACCOUNT_LABEL: &str = "pi-rust.com";
/// The largest HTML export the service accepts.
pub const MAX_SHARE_BYTES: usize = 25 * 1024 * 1024;

const CLIENT_ID: &str = "pi-cli";
const SCOPE: &str = "share";
const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const EXPIRED: &str = "The pi-rust.com sign-in code expired. Run /share again to get a new code.";
const REVOKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The share service origin, honoring `PI_SHARE_URL`.
pub fn share_url() -> String {
    pi_ai::env::process_env_value(ENV_SHARE_URL)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_SHARE_URL.to_string())
}

/// A signed-in pi-rust.com account. The token is never printed.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PiRustAccount {
    pub origin: String,
    pub token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

impl std::fmt::Debug for PiRustAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PiRustAccount")
            .field("origin", &self.origin)
            .field("token", &"<redacted>")
            .field("email", &self.email)
            .finish()
    }
}

impl PiRustAccount {
    /// Whether the account belongs to the service at `origin`.
    pub fn is_for(&self, origin: &str) -> bool {
        self.origin.trim_end_matches('/') == origin.trim_end_matches('/')
    }

    /// `pi-rust.com`, or `pi-rust.com (<email>)`.
    pub fn label(&self) -> String {
        match &self.email {
            Some(email) => format!("{ACCOUNT_LABEL} ({email})"),
            None => ACCOUNT_LABEL.to_string(),
        }
    }
}

pub fn account_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join(ACCOUNT_FILE)
}

/// The stored account, if the file exists and is valid.
pub fn load_account(agent_dir: &Path) -> Option<PiRustAccount> {
    let text = std::fs::read_to_string(account_path(agent_dir)).ok()?;
    serde_json::from_str::<PiRustAccount>(&text).ok().filter(|account| !account.token.is_empty())
}

/// The stored account when it belongs to the service at `origin`.
pub fn load_account_for(agent_dir: &Path, origin: &str) -> Option<PiRustAccount> {
    load_account(agent_dir).filter(|account| account.is_for(origin))
}

/// Writes the account file, readable only by the user on Unix.
pub fn save_account(agent_dir: &Path, account: &PiRustAccount) -> std::io::Result<()> {
    use std::io::Write;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(agent_dir)?;
    let path = account_path(agent_dir);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    // `mode` applies to a new file; the chmod narrows an existing one before the token is written.
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.mode(0o600);
        if path.exists() {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    let text = serde_json::to_string_pretty(account).map_err(std::io::Error::other)?;
    options.open(&path)?.write_all(text.as_bytes())
}

/// Removes the account file; a missing file is not an error.
pub fn delete_account(agent_dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(account_path(agent_dir)) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PiRustError {
    Cancelled,
    /// The token was revoked or expired (HTTP 401).
    Unauthorized,
    Failed(String),
}

impl std::fmt::Display for PiRustError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("Share cancelled"),
            Self::Unauthorized => f.write_str(
                "Your pi-rust.com sign-in is no longer valid, so pi removed it. Run /share again to sign in.",
            ),
            Self::Failed(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for PiRustError {}

impl From<AuthError> for PiRustError {
    fn from(error: AuthError) -> Self {
        match error {
            AuthError::Cancelled => Self::Cancelled,
            // The shared device-code poller's message when `expires_in` runs out.
            AuthError::Other(message) if message.starts_with("Device flow timed out") => Self::Failed(EXPIRED.into()),
            other => Self::Failed(other.to_string()),
        }
    }
}

impl From<PiRustError> for AuthError {
    fn from(error: PiRustError) -> Self {
        match error {
            PiRustError::Cancelled => Self::Cancelled,
            other => Self::Other(other.to_string()),
        }
    }
}

/// The device authorization response (RFC 8628 section 3.2).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct DeviceAuthorization {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    pub expires_in: u64,
    #[serde(default)]
    pub interval: Option<u64>,
}

impl DeviceAuthorization {
    /// The page to open: `verification_uri_complete` when it is a web URL, else `verification_uri`.
    pub fn browser_uri(&self) -> &str {
        self.verification_uri_complete.as_deref().filter(|uri| is_web_url(uri)).unwrap_or(&self.verification_uri)
    }
}

fn is_web_url(value: &str) -> bool {
    reqwest::Url::parse(value).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
}

/// An access token and the account email from the token endpoint.
#[derive(Clone, PartialEq, Eq)]
pub struct TokenGrant {
    pub access_token: String,
    pub email: Option<String>,
}

impl std::fmt::Debug for TokenGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenGrant").field("access_token", &"<redacted>").field("email", &self.email).finish()
    }
}

/// A created share.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PiRustShare {
    pub id: String,
    pub url: String,
}

/// Expiry and visibility of a new share, from `sessionShareExpiry` and `sessionShareVisibility`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShareOptions {
    pub expiry: SessionShareExpiry,
    pub visibility: SessionShareVisibility,
}

impl ShareOptions {
    /// The line shown under the share URL, such as `Expires in 7 days. Only you can open it.`;
    /// `None` for the defaults.
    pub fn note(&self) -> Option<String> {
        let expiry = match self.expiry {
            SessionShareExpiry::Never => None,
            SessionShareExpiry::Hour => Some("Expires in 1 hour."),
            SessionShareExpiry::Day => Some("Expires in 1 day."),
            SessionShareExpiry::Week => Some("Expires in 7 days."),
            SessionShareExpiry::Month => Some("Expires in 30 days."),
        };
        let private = (self.visibility == SessionShareVisibility::Private)
            .then_some("Only you can open it, signed in to pi-rust.com.");
        let parts: Vec<&str> = expiry.into_iter().chain(private).collect();
        (!parts.is_empty()).then(|| parts.join(" "))
    }
}

fn endpoint(origin: &str, path: &str) -> Result<reqwest::Url, PiRustError> {
    reqwest::Url::parse(origin)
        .and_then(|base| base.join(path))
        .map_err(|error| PiRustError::Failed(format!("Invalid pi-rust.com share URL {origin:?}: {error}")))
}

fn network_error(error: reqwest::Error) -> PiRustError {
    PiRustError::Failed(format!("Could not reach pi-rust.com: {error}"))
}

async fn send(request: reqwest::RequestBuilder, cancel: &CancellationToken) -> Result<Response, PiRustError> {
    let request = request.header(reqwest::header::USER_AGENT, pi_user_agent(VERSION));
    tokio::select! {
        () = cancel.cancelled() => Err(PiRustError::Cancelled),
        response = request.send() => response.map_err(network_error),
    }
}

async fn read_json(response: Response, cancel: &CancellationToken) -> Result<(StatusCode, Option<Value>), PiRustError> {
    let status = response.status();
    let body = tokio::select! {
        () = cancel.cancelled() => return Err(PiRustError::Cancelled),
        body = response.bytes() => body.map_err(network_error)?,
    };
    Ok((status, serde_json::from_slice(&body).ok()))
}

fn field<'a>(json: Option<&'a Value>, pointer: &str) -> Option<&'a str> {
    json?.pointer(pointer)?.as_str().filter(|value| !value.is_empty())
}

/// The service's `description`, else its `error` code, else the HTTP status.
fn service_reason(status: StatusCode, json: Option<&Value>) -> String {
    let fallback = || status.canonical_reason().map_or_else(|| status.as_u16().to_string(), str::to_string);
    field(json, "/description").or_else(|| field(json, "/error")).map_or_else(fallback, str::to_string)
}

/// Starts a sign-in: `POST /oauth/device_authorization`.
pub async fn start_device_authorization(
    client: &reqwest::Client,
    origin: &str,
    cancel: &CancellationToken,
) -> Result<DeviceAuthorization, PiRustError> {
    let url = endpoint(origin, "/oauth/device_authorization")?;
    let request =
        client.post(url).header("Accept", "application/json").form(&[("client_id", CLIENT_ID), ("scope", SCOPE)]);
    let (status, json) = read_json(send(request, cancel).await?, cancel).await?;
    if !status.is_success() {
        let reason = service_reason(status, json.as_ref());
        return Err(PiRustError::Failed(format!("pi-rust.com sign-in failed: {reason}")));
    }
    json.and_then(|json| serde_json::from_value::<DeviceAuthorization>(json).ok())
        .filter(|device| !device.device_code.is_empty() && is_web_url(&device.verification_uri))
        .ok_or_else(|| PiRustError::Failed("pi-rust.com sign-in failed: invalid device authorization response".into()))
}

/// One token request.
async fn request_token(
    client: &reqwest::Client,
    url: &reqwest::Url,
    device_code: &str,
    cancel: &CancellationToken,
) -> Result<DevicePoll<TokenGrant>, PiRustError> {
    let form = [("grant_type", DEVICE_CODE_GRANT), ("device_code", device_code), ("client_id", CLIENT_ID)];
    let request = client.post(url.clone()).header("Accept", "application/json").form(&form);
    let (status, json) = read_json(send(request, cancel).await?, cancel).await?;
    if status.is_success() {
        let token = field(json.as_ref(), "/access_token");
        let email = field(json.as_ref(), "/account/email").map(str::to_string);
        return match token {
            Some(token) => Ok(DevicePoll::Complete(TokenGrant { access_token: token.to_string(), email })),
            None => Err(PiRustError::Failed("pi-rust.com sign-in failed: the token response has no token".into())),
        };
    }
    Ok(match field(json.as_ref(), "/error") {
        Some("authorization_pending") => DevicePoll::Pending,
        Some("slow_down") => DevicePoll::SlowDown { interval_seconds: None },
        Some("access_denied") => DevicePoll::Failed("pi-rust.com sign-in was denied.".into()),
        Some("expired_token") => DevicePoll::Failed(EXPIRED.into()),
        _ => DevicePoll::Failed(format!("pi-rust.com sign-in failed: {}", service_reason(status, json.as_ref()))),
    })
}

/// Polls `POST /oauth/token` every `interval` seconds (5 more after `slow_down`) until the user
/// approves, denies, or the code expires after `expires_in` seconds.
pub async fn poll_device_token(
    client: &reqwest::Client,
    origin: &str,
    device: &DeviceAuthorization,
    cancel: &CancellationToken,
) -> Result<TokenGrant, PiRustError> {
    let url = endpoint(origin, "/oauth/token")?;
    let options = DeviceCodePollOptions {
        interval_seconds: device.interval.map(|seconds| seconds as f64),
        expires_in_seconds: Some(device.expires_in as f64),
        wait_before_first_poll: true,
    };
    let url = &url;
    let poll =
        move || async move { request_token(client, url, &device.device_code, cancel).await.map_err(AuthError::from) };
    Ok(poll_device_code_flow(options, cancel, poll).await?)
}

/// The email of the account that owns `token`: `GET /v1/account`.
pub async fn fetch_account_email(
    client: &reqwest::Client,
    origin: &str,
    token: &str,
    cancel: &CancellationToken,
) -> Result<Option<String>, PiRustError> {
    let request = client.get(endpoint(origin, "/v1/account")?).bearer_auth(token);
    let (status, json) = read_json(send(request, cancel).await?, cancel).await?;
    match status {
        StatusCode::UNAUTHORIZED => Err(PiRustError::Unauthorized),
        status if status.is_success() => Ok(field(json.as_ref(), "/account/email").map(str::to_string)),
        status => Err(PiRustError::Failed(format!(
            "Could not read the pi-rust.com account: {}",
            service_reason(status, json.as_ref())
        ))),
    }
}

/// Signs in to the service at `origin`: shows the code through `interaction`, then waits for the
/// user to approve it in the browser.
pub async fn sign_in(
    client: &reqwest::Client,
    origin: &str,
    interaction: &dyn AuthInteraction,
    cancel: &CancellationToken,
) -> Result<PiRustAccount, PiRustError> {
    let device = start_device_authorization(client, origin, cancel).await?;
    interaction.notify(AuthEvent::DeviceCode {
        user_code: device.user_code.clone(),
        verification_uri: device.browser_uri().to_string(),
        open_browser_on_confirm: None,
        copy_code_on_confirm: None,
        interval_seconds: device.interval,
        expires_in_seconds: Some(device.expires_in),
    });
    let grant = poll_device_token(client, origin, &device, cancel).await?;
    let email = match grant.email {
        Some(email) => Some(email),
        None => fetch_account_email(client, origin, &grant.access_token, cancel).await?,
    };
    Ok(PiRustAccount { origin: origin.to_string(), token: grant.access_token, email })
}

/// Revokes the account's token: `DELETE /v1/token`. A token the service no longer knows counts as revoked.
pub async fn revoke_token(client: &reqwest::Client, account: &PiRustAccount) -> Result<(), PiRustError> {
    let cancel = CancellationToken::new();
    let request =
        client.delete(endpoint(&account.origin, "/v1/token")?).bearer_auth(&account.token).timeout(REVOKE_TIMEOUT);
    let (status, json) = read_json(send(request, &cancel).await?, &cancel).await?;
    if status.is_success() || status == StatusCode::UNAUTHORIZED {
        return Ok(());
    }
    Err(PiRustError::Failed(format!(
        "Could not revoke the pi-rust.com token: {}",
        service_reason(status, json.as_ref())
    )))
}

/// Uploads an HTML export as a share: `POST /v1/shares?title=<title>`.
pub async fn upload_share(
    client: &reqwest::Client,
    account: &PiRustAccount,
    title: &str,
    options: ShareOptions,
    html: Vec<u8>,
    cancel: &CancellationToken,
) -> Result<PiRustShare, PiRustError> {
    let failed = |reason: String| PiRustError::Failed(format!("Failed to share on pi-rust.com: {reason}"));
    if html.len() > MAX_SHARE_BYTES {
        let mib = html.len() as f64 / (1024.0 * 1024.0);
        return Err(failed(format!("the session export is {mib:.1} MiB, more than the 25 MiB limit")));
    }
    let mut url = endpoint(&account.origin, "/v1/shares")?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("title", title);
        // Defaults are left out, so older servers get the request they know.
        if let Some(seconds) = options.expiry.seconds() {
            query.append_pair("expires_in", &seconds.to_string());
        }
        if options.visibility == SessionShareVisibility::Private {
            query.append_pair("visibility", "private");
        }
    }
    let request = client
        .post(url)
        .bearer_auth(&account.token)
        .header(reqwest::header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(html);
    let (status, json) = read_json(send(request, cancel).await?, cancel).await?;
    if status == StatusCode::UNAUTHORIZED {
        return Err(PiRustError::Unauthorized);
    }
    let ok = json.as_ref().and_then(|json| json.get("ok")).and_then(Value::as_bool) == Some(true);
    match (field(json.as_ref(), "/share/id"), field(json.as_ref(), "/share/url")) {
        (Some(id), Some(url)) if status.is_success() && ok => Ok(PiRustShare { id: id.into(), url: url.into() }),
        _ => Err(failed(service_reason(status, json.as_ref()))),
    }
}
