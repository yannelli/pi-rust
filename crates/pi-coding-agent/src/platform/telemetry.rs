//! Install telemetry: an anonymous version ping after the changelog detects an install or an
//! update (upstream `core/telemetry.ts` and `reportInstallTelemetry`). `PI_TELEMETRY`
//! overrides the `enableInstallTelemetry` setting. Unlike upstream, an unset setting means off.

use std::time::Duration;

use super::http::pi_user_agent;

pub const REPORT_INSTALL_URL: &str = "https://pi.dev/api/report-install";
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

/// The ping URL for `version` under `endpoint`.
pub fn report_install_url(endpoint: &str, version: &str) -> String {
    let mut url = reqwest::Url::parse(endpoint).unwrap_or_else(|_| reqwest::Url::parse(REPORT_INSTALL_URL).unwrap());
    url.query_pairs_mut().clear().append_pair("version", version);
    url.to_string()
}

/// Send the install ping unless offline or disabled. Failures are ignored; returns whether a
/// response arrived.
pub async fn report_install_telemetry(
    client: &reqwest::Client,
    endpoint: &str,
    version: &str,
    enabled: bool,
    offline: bool,
) -> bool {
    if offline || !enabled {
        return false;
    }
    let request = client.get(report_install_url(endpoint, version)).header("User-Agent", pi_user_agent(version));
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
    }

    #[tokio::test]
    async fn sends_the_version_only_when_allowed() {
        let server = TestServer::start(|_| TestReply::status(200, "")).await;
        let endpoint = format!("{}/api/report-install", server.url);
        let http = client();
        assert!(!report_install_telemetry(&http, &endpoint, "1.2.3", false, false).await);
        assert!(!report_install_telemetry(&http, &endpoint, "1.2.3", true, true).await);
        assert!(server.requests().is_empty());
        assert!(report_install_telemetry(&http, &endpoint, "1.2.3-beta+1", true, false).await);
        let request = &server.requests()[0];
        assert_eq!(request.path(), "/api/report-install");
        assert_eq!(request.query("version").as_deref(), Some("1.2.3-beta+1"));
        assert!(request.header("user-agent").unwrap().starts_with("pi/1.2.3-beta+1 "));
    }
}
