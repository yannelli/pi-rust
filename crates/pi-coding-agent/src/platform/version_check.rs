//! Version check against the latest GitHub release of pi-rust, the source `pi update` installs
//! from (upstream `utils/version-check.ts` asks pi.dev, which reports the TypeScript releases).
//! `PI_OFFLINE` disables every request; `PI_SKIP_VERSION_CHECK` disables only the startup check.

use std::cmp::Ordering;
use std::time::Duration;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::http::{FetchError, FetchRetryOptions, fetch_with_retry, pi_user_agent};
use crate::update::GitHubRelease;

pub const LATEST_VERSION_URL: &str = "https://api.github.com/repos/yannelli/pi-rust/releases/latest";
const DEFAULT_TIMEOUT: Duration = Duration::from_millis(10_000);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LatestPiRelease {
    pub version: String,
    pub package_name: Option<String>,
    /// Markdown shown before updating.
    pub note: Option<String>,
}

fn parse_version(value: &str) -> Option<semver::Version> {
    let trimmed = value.trim();
    let bare = trimmed.strip_prefix('v').or_else(|| trimmed.strip_prefix('=')).unwrap_or(trimmed);
    semver::Version::parse(bare).ok()
}

/// Semantic version order, or `None` when either side is not a valid version.
pub fn compare_package_versions(left: &str, right: &str) -> Option<Ordering> {
    Some(parse_version(left)?.cmp_precedence(&parse_version(right)?))
}

/// Newer by semantic version; any difference counts when a version does not parse.
pub fn is_newer_package_version(candidate: &str, current: &str) -> bool {
    match compare_package_versions(candidate, current) {
        Some(order) => order == Ordering::Greater,
        None => candidate.trim() != current.trim(),
    }
}

#[derive(Clone, Debug)]
pub struct VersionCheckOptions {
    /// The GitHub "latest release" endpoint; `PI_GITHUB_API_URL` replaces api.github.com.
    pub url: String,
    pub timeout: Duration,
    /// Retry transient failures twice, as `pi update` does.
    pub retry: bool,
    pub offline: bool,
    pub client: reqwest::Client,
    /// `GITHUB_TOKEN`, for higher API rate limits.
    pub token: Option<String>,
}

impl Default for VersionCheckOptions {
    fn default() -> Self {
        let api_url = crate::update::github_api_url();
        let url = match api_url.as_str() {
            crate::update::release::GITHUB_API_URL => LATEST_VERSION_URL.into(),
            _ => format!("{api_url}/repos/{}/releases/latest", crate::update::GITHUB_REPO),
        };
        Self {
            url,
            timeout: DEFAULT_TIMEOUT,
            retry: false,
            offline: crate::config::offline(),
            client: pi_ai::http::shared_client(),
            token: pi_ai::env::process_env_value("GITHUB_TOKEN").filter(|token| !token.is_empty()),
        }
    }
}

/// The latest release on GitHub, or `None` offline, on an HTTP error, or without a tag.
pub async fn get_latest_pi_release(
    current_version: &str,
    options: &VersionCheckOptions,
) -> Result<Option<LatestPiRelease>, FetchError> {
    if options.offline {
        return Ok(None);
    }
    let retry = FetchRetryOptions {
        max_retries: if options.retry { 2 } else { 0 },
        timeout: Some(options.timeout),
        ..FetchRetryOptions::default()
    };
    let agent = pi_user_agent(current_version);
    let build = || {
        let request = options
            .client
            .get(&options.url)
            .header("User-Agent", agent.as_str())
            .header("accept", "application/vnd.github+json");
        match &options.token {
            Some(token) => request.header("Authorization", format!("Bearer {token}")),
            None => request,
        }
    };
    let response = fetch_with_retry(build, &retry, &CancellationToken::new()).await?;
    if !response.status().is_success() {
        return Ok(None);
    }
    let data: Value = match tokio::time::timeout(options.timeout, response.json()).await {
        Ok(Ok(data)) => data,
        Ok(Err(error)) => return Err(FetchError::Request(error)),
        Err(_) => return Err(FetchError::TimedOut),
    };
    let Some(release) = GitHubRelease::from_json(&data).filter(|release| !release.prerelease) else { return Ok(None) };
    let version = release.version().trim();
    if version.is_empty() {
        return Ok(None);
    }
    Ok(Some(LatestPiRelease { version: version.to_string(), package_name: None, note: None }))
}

pub async fn get_latest_pi_version(
    current_version: &str,
    options: &VersionCheckOptions,
) -> Result<Option<String>, FetchError> {
    Ok(get_latest_pi_release(current_version, options).await?.map(|release| release.version))
}

/// Whether `PI_SKIP_VERSION_CHECK` is set.
pub fn version_check_skipped() -> bool {
    pi_ai::env::process_env_value("PI_SKIP_VERSION_CHECK").is_some_and(|value| !value.is_empty())
}

/// The automatic startup check: one request, errors ignored, only a newer release returned.
pub async fn check_for_new_pi_version(
    current_version: &str,
    options: &VersionCheckOptions,
    skip: bool,
) -> Option<LatestPiRelease> {
    if skip {
        return None;
    }
    let options = VersionCheckOptions { retry: false, ..options.clone() };
    let release = get_latest_pi_release(current_version, &options).await.ok()??;
    is_newer_package_version(&release.version, current_version).then_some(release)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    use serde_json::json;

    use super::*;
    use crate::platform::test_http::{TestReply, TestServer, client};

    const PATH: &str = "/repos/yannelli/pi-rust/releases/latest";

    fn options(server: &TestServer) -> VersionCheckOptions {
        VersionCheckOptions {
            url: format!("{}{PATH}", server.url),
            timeout: Duration::from_secs(5),
            retry: false,
            offline: false,
            client: client(),
            token: None,
        }
    }

    fn release(tag: &str) -> serde_json::Value {
        json!({"tag_name": tag, "prerelease": false, "body": "## Changes", "assets": []})
    }

    #[test]
    fn compares_package_versions() {
        assert_eq!(compare_package_versions("0.70.6", "0.70.5"), Some(Ordering::Greater));
        assert_eq!(compare_package_versions("0.70.5", "0.70.5"), Some(Ordering::Equal));
        assert_eq!(compare_package_versions("0.70.4", "0.70.5"), Some(Ordering::Less));
        assert_eq!(compare_package_versions("5.0.0-beta.20", "5.0.0-beta.9"), Some(Ordering::Greater));
        assert_eq!(compare_package_versions("v1.0.0", "1.0.0"), Some(Ordering::Equal));
        assert_eq!(compare_package_versions("latest", "1.0.0"), None);
        assert!(!is_newer_package_version("0.70.5", "0.70.5"));
        assert!(is_newer_package_version("0.70.6", "0.70.5"));
        assert!(is_newer_package_version("nightly", "0.70.5"));
    }

    #[tokio::test]
    async fn returns_only_newer_github_releases() {
        let server = TestServer::start(|_| TestReply::json(&release("v1.2.3"))).await;
        let mut options = options(&server);
        assert_eq!(check_for_new_pi_version("1.2.3", &options, false).await, None);
        let latest = check_for_new_pi_version("1.2.2", &options, false).await.unwrap();
        assert_eq!(latest, LatestPiRelease { version: "1.2.3".into(), package_name: None, note: None });
        let request = &server.requests()[0];
        assert!(request.header("user-agent").unwrap().starts_with("pi/1.2.3 "));
        assert_eq!(request.header("accept"), Some("application/vnd.github+json"));
        assert_eq!(request.path(), PATH);
        assert_eq!(request.header("authorization"), None);
        options.token = Some("gh-token".into());
        check_for_new_pi_version("1.2.2", &options, false).await.unwrap();
        assert_eq!(server.requests()[2].header("authorization"), Some("Bearer gh-token"));
    }

    #[tokio::test]
    async fn explicit_requests_retry_but_startup_checks_do_not() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let server = TestServer::start(move |_| {
            if counter.fetch_add(1, AtomicOrdering::SeqCst) < 2 {
                TestReply::status(503, "down")
            } else {
                TestReply::json(&release("v1.2.4"))
            }
        })
        .await;
        let mut options = options(&server);
        assert_eq!(check_for_new_pi_version("1.2.3", &options, false).await, None);
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
        calls.store(0, AtomicOrdering::SeqCst);
        options.retry = true;
        assert_eq!(get_latest_pi_version("1.2.3", &options).await.unwrap().as_deref(), Some("1.2.4"));
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 3);
    }

    #[tokio::test]
    async fn skip_and_offline_avoid_requests() {
        let server = TestServer::start(|_| TestReply::json(&release("9.9.9"))).await;
        let mut options = options(&server);
        assert_eq!(check_for_new_pi_version("1.0.0", &options, true).await, None);
        assert!(server.requests().is_empty());
        assert_eq!(get_latest_pi_version("1.0.0", &options).await.unwrap().as_deref(), Some("9.9.9"));
        options.offline = true;
        assert_eq!(get_latest_pi_version("1.0.0", &options).await.unwrap(), None);
        assert_eq!(server.requests().len(), 1);
    }

    #[tokio::test]
    async fn missing_tags_prereleases_and_http_errors_return_none() {
        let server = TestServer::start(|request| match request.query("case").as_deref() {
            Some("blank") => TestReply::json(&json!({"name": "no tag"})),
            Some("pre") => TestReply::json(&json!({"tag_name": "v2.0.0-rc.1", "prerelease": true})),
            _ => TestReply::status(404, "missing"),
        })
        .await;
        let mut options = options(&server);
        for case in ["blank", "pre"] {
            options.url = format!("{}{PATH}?case={case}", server.url);
            assert_eq!(get_latest_pi_release("1.0.0", &options).await.unwrap(), None);
        }
        options.url = format!("{}{PATH}", server.url);
        assert_eq!(get_latest_pi_release("1.0.0", &options).await.unwrap(), None);
    }
}
