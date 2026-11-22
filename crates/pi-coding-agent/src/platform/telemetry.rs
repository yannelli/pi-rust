//! Install telemetry: an anonymous version ping after the changelog detects an install or an
//! update (upstream `core/telemetry.ts` and `reportInstallTelemetry`). `PI_TELEMETRY`
//! overrides the `enableInstallTelemetry` setting. Unlike upstream, an unset setting means off,
//! and the ping goes to pi-rust.com (or `PI_TELEMETRY_URL`) instead of upstream's pi.dev.

use std::time::Duration;

use super::http::pi_user_agent;

pub const ENV_TELEMETRY_URL: &str = "PI_TELEMETRY_URL";
pub const DEFAULT_TELEMETRY_URL: &str = "https://pi-rust.com";
const REPORT_INSTALL_PATH: &str = "/api/report-install";
const REPORT_TIMEOUT: Duration = Duration::from_millis(5000);

/// `1`, `true`, or `yes` in any case.
pub fn is_truthy_env_flag(value: Option<&str>) -> bool {
    value.is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("yes"))
}

/// `PI_TELEMETRY` when set, else the explicit `enableInstallTelemetry` value, else off.
pub fn is_install_telemetry_enabled(setting: Option<bool>, telemetry_env: Option<&str>) -> bool {
    match telemetry_env {
        Some(value) => is_truthy_env_flag(Some(value)),
        None => setting.unwrap_or(false),
    }
}

/// [`is_install_telemetry_enabled`] for the merged settings and the process environment.
pub fn install_telemetry_enabled(settings: &crate::settings::SettingsManager) -> bool {
    let env = pi_ai::env::process_env_value("PI_TELEMETRY");
    is_install_telemetry_enabled(settings.settings().enable_install_telemetry, env.as_deref())
}

/// The telemetry collector origin, honoring `PI_TELEMETRY_URL`.
pub fn telemetry_url() -> String {
    pi_ai::env::process_env_value(ENV_TELEMETRY_URL)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_TELEMETRY_URL.to_string())
}

/// The ping URL for `version` on the collector at `origin`; `None` when `origin` is not a URL.
pub fn report_install_url(origin: &str, version: &str) -> Option<String> {
    let mut url = reqwest::Url::parse(origin).and_then(|base| base.join(REPORT_INSTALL_PATH)).ok()?;
    url.query_pairs_mut().clear().append_pair("version", version);
    Some(url.to_string())
}

/// Send the install ping to the collector at `origin` unless offline or disabled. Failures are
/// ignored; returns whether a response arrived.
pub async fn report_install_telemetry(
    client: &reqwest::Client,
    origin: &str,
    version: &str,
    enabled: bool,
    offline: bool,
) -> bool {
    if offline || !enabled {
        return false;
    }
    let Some(url) = report_install_url(origin, version) else {
        return false;
    };
    let request = client.get(url).header("User-Agent", pi_user_agent(version));
    matches!(tokio::time::timeout(REPORT_TIMEOUT, request.send()).await, Ok(Ok(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::test_http::{TestReply, TestServer, client};

    #[test]
    fn environment_overrides_the_setting_and_default_is_off() {
        assert!(!is_install_telemetry_enabled(None, None));
        assert!(is_install_telemetry_enabled(Some(true), None));
        assert!(!is_install_telemetry_enabled(Some(true), Some("0")));
        assert!(is_install_telemetry_enabled(Some(false), Some("YES")));
        assert!(!is_install_telemetry_enabled(None, Some("")));
        let settings = crate::settings::SettingsManager::in_memory(serde_json::json!({}));
        assert_eq!(settings.settings().enable_install_telemetry, None);
        assert!(!settings.enable_install_telemetry());
    }

    #[test]
    fn pings_pi_rust_com_and_never_upstream() {
        let url = report_install_url(DEFAULT_TELEMETRY_URL, "1.2.3").unwrap();
        assert_eq!(url, "https://pi-rust.com/api/report-install?version=1.2.3");
        let custom = report_install_url("https://collector.test/base/", "1.2.3-beta+1").unwrap();
        assert_eq!(custom, "https://collector.test/api/report-install?version=1.2.3-beta%2B1");
        assert_eq!(report_install_url("not a url", "1.2.3"), None);
    }

    #[tokio::test]
    async fn sends_the_version_only_when_allowed() {
        let server = TestServer::start(|_| TestReply::status(200, "")).await;
        let http = client();
        assert!(!report_install_telemetry(&http, &server.url, "1.2.3", false, false).await);
        assert!(!report_install_telemetry(&http, &server.url, "1.2.3", true, true).await);
        assert!(!report_install_telemetry(&http, "not a url", "1.2.3", true, false).await);
        assert!(server.requests().is_empty());
        assert!(report_install_telemetry(&http, &server.url, "1.2.3-beta+1", true, false).await);
        let request = &server.requests()[0];
        assert_eq!(request.path(), "/api/report-install");
        assert_eq!(request.query("version").as_deref(), Some("1.2.3-beta+1"));
        assert!(request.header("user-agent").unwrap().starts_with("pi/1.2.3-beta+1 "));
    }
}
