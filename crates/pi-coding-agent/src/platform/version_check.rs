//! Version check against pi.dev (upstream `utils/version-check.ts`). `PI_OFFLINE` disables
//! every request; `PI_SKIP_VERSION_CHECK` disables only the automatic startup check.

use std::cmp::Ordering;
use std::time::Duration;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::http::{FetchError, FetchRetryOptions, fetch_with_retry, pi_user_agent};

pub const LATEST_VERSION_URL: &str = "https://pi.dev/api/latest-version";
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
    pub url: String,
    pub timeout: Duration,
    /// Retry transient failures twice, as `pi update` does.
    pub retry: bool,
    pub offline: bool,
    pub client: reqwest::Client,
}

impl Default for VersionCheckOptions {
    fn default() -> Self {
        Self {
            url: LATEST_VERSION_URL.into(),
            timeout: DEFAULT_TIMEOUT,
            retry: false,
            offline: crate::config::offline(),
            client: pi_ai::http::shared_client(),
        }
    }
}

fn non_empty(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::trim).filter(|text| !text.is_empty()).map(String::from)
}

/// The release pi.dev reports, or `None` offline, on an HTTP error, or without a version.
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
    let build =
        || options.client.get(&options.url).header("User-Agent", agent.as_str()).header("accept", "application/json");
    let response = fetch_with_retry(build, &retry, &CancellationToken::new()).await?;
    if !response.status().is_success() {
        return Ok(None);
    }
    let data: Value = match tokio::time::timeout(options.timeout, response.json()).await {
        Ok(Ok(data)) => data,
        Ok(Err(error)) => return Err(FetchError::Request(error)),
        Err(_) => return Err(FetchError::TimedOut),
    };
    let Some(version) = non_empty(data.get("version")) else { return Ok(None) };
    Ok(Some(LatestPiRelease {
        version,
        package_name: non_empty(data.get("packageName")),
        note: non_empty(data.get("note")),
    }))
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

    fn options(server: &TestServer) -> VersionCheckOptions {
        VersionCheckOptions {
            url: format!("{}/api/latest-version", server.url),
            timeout: Duration::from_secs(5),
            retry: false,
            offline: false,
            client: client(),
        }
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
    async fn returns_only_newer_versions_with_metadata() {
        let server = TestServer::start(|_| {
            TestReply::json(&json!({"version": "1.2.3", "packageName": " @new-scope/pi ", "note": " **Read this** "}))
        })
        .await;
        let options = options(&server);
        assert_eq!(check_for_new_pi_version("1.2.3", &options, false).await, None);
        let release = check_for_new_pi_version("1.2.2", &options, false).await.unwrap();
        assert_eq!(release.version, "1.2.3");
        assert_eq!(release.package_name.as_deref(), Some("@new-scope/pi"));
        assert_eq!(release.note.as_deref(), Some("**Read this**"));
        let request = &server.requests()[0];
        assert!(request.header("user-agent").unwrap().starts_with("pi/1.2.3 "));
        assert_eq!(request.header("accept"), Some("application/json"));
        assert_eq!(request.path(), "/api/latest-version");
    }

    #[tokio::test]
    async fn explicit_requests_retry_but_startup_checks_do_not() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let server = TestServer::start(move |_| {
            if counter.fetch_add(1, AtomicOrdering::SeqCst) < 2 {
                TestReply::status(503, "down")
            } else {
                TestReply::json(&json!({"version": "1.2.4"}))
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
        let server = TestServer::start(|_| TestReply::json(&json!({"version": "9.9.9"}))).await;
        let mut options = options(&server);
        assert_eq!(check_for_new_pi_version("1.0.0", &options, true).await, None);
        assert!(server.requests().is_empty());
        assert_eq!(get_latest_pi_version("1.0.0", &options).await.unwrap().as_deref(), Some("9.9.9"));
        options.offline = true;
        assert_eq!(get_latest_pi_version("1.0.0", &options).await.unwrap(), None);
        assert_eq!(server.requests().len(), 1);
    }

    #[tokio::test]
    async fn missing_versions_and_http_errors_return_none() {
        let server = TestServer::start(|request| match request.query("case").as_deref() {
            Some("blank") => TestReply::json(&json!({"version": "  "})),
            _ => TestReply::status(404, "missing"),
        })
        .await;
        let mut options = options(&server);
        options.url = format!("{}/api/latest-version?case=blank", server.url);
        assert_eq!(get_latest_pi_release("1.0.0", &options).await.unwrap(), None);
        options.url = format!("{}/api/latest-version", server.url);
        assert_eq!(get_latest_pi_release("1.0.0", &options).await.unwrap(), None);
    }
}
