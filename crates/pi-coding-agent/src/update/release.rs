//! GitHub releases of pi-rust: the latest release, the archive for this build's target, and
//! its `SHA256SUMS` entry.

use std::fmt;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::platform::http::{FetchRetryOptions, VERSION, fetch_with_retry, format_network_error, pi_user_agent};

pub const GITHUB_REPO: &str = "yannelli/pi-rust";
pub const GITHUB_API_URL: &str = "https://api.github.com";
pub const CHECKSUMS_ASSET: &str = "SHA256SUMS";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateError {
    Cancelled,
    Network(String),
    /// The release, an asset, or a checksum is missing or does not match.
    Release(String),
    Io(String),
    /// This installation is updated another way.
    Unsupported(String),
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("Update cancelled"),
            Self::Network(message) | Self::Release(message) | Self::Io(message) | Self::Unsupported(message) => {
                f.write_str(message)
            }
        }
    }
}

impl std::error::Error for UpdateError {}

impl From<std::io::Error> for UpdateError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseAsset {
    pub name: String,
    pub url: String,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitHubRelease {
    pub tag_name: String,
    pub prerelease: bool,
    /// Release notes (Markdown).
    pub body: Option<String>,
    pub assets: Vec<ReleaseAsset>,
}

impl GitHubRelease {
    /// The tag without its `v` prefix.
    pub fn version(&self) -> &str {
        self.tag_name.strip_prefix('v').unwrap_or(&self.tag_name)
    }

    pub fn asset(&self, name: &str) -> Option<&ReleaseAsset> {
        self.assets.iter().find(|asset| asset.name == name)
    }

    pub fn from_json(value: &Value) -> Option<Self> {
        let tag_name = value.get("tag_name")?.as_str()?.to_string();
        let assets = value.get("assets").and_then(Value::as_array).into_iter().flatten().filter_map(|asset| {
            Some(ReleaseAsset {
                name: asset.get("name")?.as_str()?.to_string(),
                url: asset.get("browser_download_url")?.as_str()?.to_string(),
                size: asset.get("size").and_then(Value::as_u64).unwrap_or(0),
            })
        });
        Some(Self {
            tag_name,
            prerelease: value.get("prerelease").and_then(Value::as_bool).unwrap_or(false),
            body: value.get("body").and_then(Value::as_str).filter(|body| !body.trim().is_empty()).map(String::from),
            assets: assets.collect(),
        })
    }
}

/// Target triple of the release archive that matches this build.
pub fn release_target() -> Option<&'static str> {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("x86_64-unknown-linux-musl")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some("aarch64-unknown-linux-musl")
    } else if cfg!(all(target_os = "linux", target_arch = "arm")) {
        Some("armv7-unknown-linux-musleabihf")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("x86_64-apple-darwin")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("aarch64-apple-darwin")
    } else if cfg!(all(windows, target_arch = "x86_64")) {
        Some("x86_64-pc-windows-msvc")
    } else if cfg!(all(windows, target_arch = "aarch64")) {
        Some("aarch64-pc-windows-msvc")
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveKind {
    TarGz,
    Zip,
}

/// `pi-<tag>-<target>.zip` for Windows targets, `.tar.gz` otherwise, as the release workflow packs them.
pub fn archive_name(tag: &str, target: &str) -> (String, ArchiveKind) {
    if target.contains("windows") {
        (format!("pi-{tag}-{target}.zip"), ArchiveKind::Zip)
    } else {
        (format!("pi-{tag}-{target}.tar.gz"), ArchiveKind::TarGz)
    }
}

pub fn binary_name(target: &str) -> &'static str {
    if target.contains("windows") { "pi.exe" } else { "pi" }
}

/// The archive for `target` and the checksum list of `release`.
pub fn select_assets(
    release: &GitHubRelease,
    target: &str,
) -> Result<(ReleaseAsset, ArchiveKind, ReleaseAsset), UpdateError> {
    let (name, kind) = archive_name(&release.tag_name, target);
    let archive = release.asset(&name).ok_or_else(|| {
        UpdateError::Release(format!("Release {} has no archive for {target} ({name})", release.tag_name))
    })?;
    let sums = release
        .asset(CHECKSUMS_ASSET)
        .ok_or_else(|| UpdateError::Release(format!("Release {} has no {CHECKSUMS_ASSET}", release.tag_name)))?;
    Ok((archive.clone(), kind, sums.clone()))
}

/// Where releases come from.
#[derive(Clone, Debug)]
pub struct ReleaseSource {
    pub api_url: String,
    pub repo: String,
    pub client: reqwest::Client,
    /// `GITHUB_TOKEN`, for higher API rate limits.
    pub token: Option<String>,
}

impl Default for ReleaseSource {
    fn default() -> Self {
        Self {
            api_url: GITHUB_API_URL.into(),
            repo: GITHUB_REPO.into(),
            client: pi_ai::http::shared_client(),
            token: pi_ai::env::process_env_value("GITHUB_TOKEN").filter(|token| !token.is_empty()),
        }
    }
}

impl ReleaseSource {
    fn request(&self, url: &str, accept: &str) -> reqwest::RequestBuilder {
        let request = self.client.get(url).header("accept", accept).header("User-Agent", pi_user_agent(VERSION));
        match &self.token {
            Some(token) if url.starts_with(&self.api_url) => request.header("Authorization", format!("Bearer {token}")),
            _ => request,
        }
    }

    /// GET `url` with retries; the response body on success.
    pub async fn download(&self, url: &str, accept: &str, cancel: &CancellationToken) -> Result<Vec<u8>, UpdateError> {
        let options = FetchRetryOptions::default();
        let response =
            fetch_with_retry(|| self.request(url, accept), &options, cancel).await.map_err(|error| match error {
                crate::platform::http::FetchError::Cancelled => UpdateError::Cancelled,
                other => UpdateError::Network(format!("Could not download {url}: {other}")),
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(UpdateError::Network(format!("Could not download {url}: HTTP {}", status.as_u16())));
        }
        let body = tokio::select! {
            _ = cancel.cancelled() => return Err(UpdateError::Cancelled),
            body = response.bytes() => body,
        };
        body.map(|bytes| bytes.to_vec()).map_err(|error| {
            UpdateError::Network(format!("Could not download {url}: {}", format_network_error(&error)))
        })
    }

    /// The latest published release (GitHub excludes drafts and pre-releases).
    pub async fn latest_release(&self, cancel: &CancellationToken) -> Result<GitHubRelease, UpdateError> {
        let url = format!("{}/repos/{}/releases/latest", self.api_url.trim_end_matches('/'), self.repo);
        let body = self.download(&url, "application/vnd.github+json", cancel).await?;
        let value: Value = serde_json::from_slice(&body)
            .map_err(|error| UpdateError::Release(format!("Invalid release response: {error}")))?;
        GitHubRelease::from_json(&value).ok_or_else(|| UpdateError::Release("Invalid release response".into()))
    }
}
