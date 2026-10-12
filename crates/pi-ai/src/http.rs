//! HTTP transport shared by every provider: proxy resolution (`HTTP_PROXY`, `HTTPS_PROXY`,
//! `ALL_PROXY`, `NO_PROXY`, and the `httpProxy` setting), idle timeouts, and pooled
//! connections. Port of upstream `utils/node-http-proxy.ts` and `core/http-dispatcher.ts`.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::Duration;

use reqwest::Url;

/// Default idle timeout for response headers and body reads (5 minutes).
pub const DEFAULT_HTTP_IDLE_TIMEOUT_MS: u64 = 300_000;

pub const UNSUPPORTED_PROXY_PROTOCOL_MESSAGE: &str =
    "Unsupported proxy protocol. SOCKS and PAC proxy URLs are not supported; use an HTTP or HTTPS proxy URL.";

/// Process-wide transport settings, usually from the coding agent settings file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HttpSettings {
    /// `httpProxy` setting: proxy for HTTP and HTTPS requests when the environment names none.
    pub http_proxy: Option<String>,
    /// Idle timeout for response headers and each body read; `Some(0)` disables it.
    pub idle_timeout_ms: Option<u64>,
}

impl HttpSettings {
    pub fn idle_timeout(&self) -> Option<Duration> {
        match self.idle_timeout_ms.unwrap_or(DEFAULT_HTTP_IDLE_TIMEOUT_MS) {
            0 => None,
            millis => Some(Duration::from_millis(millis)),
        }
    }

    fn proxy_setting(&self) -> Option<&str> {
        self.http_proxy.as_deref().map(str::trim).filter(|proxy| !proxy.is_empty())
    }
}

/// Idle timeout choices offered by the settings UI, as `(label, milliseconds)`.
pub const HTTP_IDLE_TIMEOUT_CHOICES: [(&str, u64); 5] =
    [("30 sec", 30_000), ("1 min", 60_000), ("2 min", 120_000), ("5 min", 300_000), ("disabled", 0)];

/// Parse an idle timeout setting: milliseconds as a number or string, or `disabled`.
pub fn parse_http_idle_timeout_ms(value: &serde_json::Value) -> Option<u64> {
    match value {
        serde_json::Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.eq_ignore_ascii_case("disabled") {
                return Some(0);
            }
            let number: f64 = trimmed.parse().ok()?;
            parse_http_idle_timeout_ms(&serde_json::json!(number))
        }
        serde_json::Value::Number(number) => {
            let millis = number.as_f64()?;
            (millis.is_finite() && millis >= 0.0).then(|| millis.floor() as u64)
        }
        _ => None,
    }
}

/// Label for an idle timeout: a choice label, or seconds.
pub fn format_http_idle_timeout_ms(timeout_ms: u64) -> String {
    HTTP_IDLE_TIMEOUT_CHOICES
        .iter()
        .find(|(_, millis)| *millis == timeout_ms)
        .map_or_else(|| format!("{} sec", timeout_ms as f64 / 1000.0), |(label, _)| label.to_string())
}

static SETTINGS: LazyLock<RwLock<HttpSettings>> = LazyLock::new(|| RwLock::new(HttpSettings::default()));
static SHARED: LazyLock<RwLock<Option<reqwest::Client>>> = LazyLock::new(|| RwLock::new(None));
/// Proxy-related variables of a scoped environment, as a cache key.
type ProxyEnvKey = Vec<(String, String)>;
static SCOPED: LazyLock<Mutex<HashMap<ProxyEnvKey, reqwest::Client>>> = LazyLock::new(Default::default);

/// Replace the transport settings. Clients built afterwards use them.
pub fn configure_http(settings: HttpSettings) {
    *SETTINGS.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = settings;
    *SHARED.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    SCOPED.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clear();
}

pub fn http_settings() -> HttpSettings {
    SETTINGS.read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
}

/// The pooled client for requests without provider-scoped proxy variables.
pub fn shared_client() -> reqwest::Client {
    if let Some(client) = SHARED.read().unwrap_or_else(|poisoned| poisoned.into_inner()).as_ref() {
        return client.clone();
    }
    let client = build_client(Arc::new(BTreeMap::new()));
    SHARED.write().unwrap_or_else(|poisoned| poisoned.into_inner()).get_or_insert(client).clone()
}

/// Proxy-related variable names; lookups try lowercase first, as upstream does.
const PROXY_KEYS: [&str; 4] = ["http_proxy", "https_proxy", "all_proxy", "no_proxy"];

/// A pooled client for a request with provider-scoped `env`. Clients are cached per
/// distinct proxy configuration so connections are reused across requests.
pub fn client_for_env(env: &BTreeMap<String, String>) -> reqwest::Client {
    let key: ProxyEnvKey = env
        .iter()
        .filter(|(name, _)| PROXY_KEYS.contains(&name.to_ascii_lowercase().as_str()))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    if key.is_empty() {
        return shared_client();
    }
    let mut cache = SCOPED.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(client) = cache.get(&key) {
        return client.clone();
    }
    let client = build_client(Arc::new(key.iter().cloned().collect()));
    cache.insert(key, client.clone());
    client
}

fn build_client(env: Arc<BTreeMap<String, String>>) -> reqwest::Client {
    let settings = http_settings();
    let proxy = reqwest::Proxy::custom(move |url| resolve_proxy_url(url.as_str(), &env).ok().flatten());
    let mut builder = reqwest::Client::builder()
        .user_agent(concat!("pi-rust/", env!("CARGO_PKG_VERSION")))
        .proxy(proxy)
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_keepalive(Duration::from_secs(60))
        .connect_timeout(Duration::from_secs(30));
    if let Some(idle) = settings.idle_timeout() {
        builder = builder.read_timeout(idle);
    }
    builder.build().expect("HTTP client configuration is valid")
}

fn default_port(scheme: &str) -> u16 {
    match scheme {
        "ftp" => 21,
        "gopher" => 70,
        "http" | "ws" => 80,
        "https" | "wss" => 443,
        _ => 0,
    }
}

/// `key` from scoped `env` (lowercase, then uppercase), then the process environment.
fn proxy_env(key: &str, env: &BTreeMap<String, String>) -> String {
    let (lower, upper) = (key.to_ascii_lowercase(), key.to_ascii_uppercase());
    let non_empty = |value: Option<String>| value.filter(|value| !value.is_empty());
    non_empty(env.get(&lower).cloned())
        .or_else(|| non_empty(env.get(&upper).cloned()))
        .or_else(|| non_empty(std::env::var(&lower).ok()))
        .or_else(|| non_empty(std::env::var(&upper).ok()))
        .unwrap_or_default()
}

fn strip_brackets(host: &str) -> &str {
    host.strip_prefix('[').and_then(|host| host.strip_suffix(']')).unwrap_or(host)
}

fn parse_no_proxy_entry(entry: &str) -> Option<(String, u16)> {
    let trimmed = entry.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('[')
        && let Some(closing) = trimmed.find(']')
    {
        let host = trimmed[1..closing].to_string();
        let port = trimmed[closing + 1..].strip_prefix(':').map(|port| port.parse().unwrap_or(0)).unwrap_or(0);
        return Some((host, port));
    }
    if trimmed.matches(':').count() > 1 {
        return Some((trimmed, 0));
    }
    if let Some((host, port)) = trimmed.split_once(':')
        && let Ok(port) = port.parse::<u16>()
    {
        return Some((host.to_string(), port));
    }
    Some((trimmed, 0))
}

fn should_proxy_hostname(hostname: &str, port: u16, env: &BTreeMap<String, String>) -> bool {
    let no_proxy = proxy_env("no_proxy", env).to_ascii_lowercase();
    if no_proxy.is_empty() {
        return true;
    }
    if no_proxy == "*" {
        return false;
    }
    let target = strip_brackets(&hostname.to_ascii_lowercase()).to_string();
    no_proxy.split([',', ' ', '\t', '\n']).all(|entry| {
        let Some((host, entry_port)) = parse_no_proxy_entry(entry) else { return true };
        if entry_port != 0 && entry_port != port {
            return true;
        }
        let host = strip_brackets(&host);
        let domain = host
            .strip_prefix("*.")
            .or_else(|| host.strip_prefix('.'))
            .or_else(|| host.strip_prefix('*'))
            .unwrap_or(host);
        if domain.is_empty() {
            return true;
        }
        target != domain && !target.ends_with(&format!(".{domain}"))
    })
}

fn proxy_for_url(target: &str, env: &BTreeMap<String, String>) -> String {
    let Ok(url) = Url::parse(target) else { return String::new() };
    let Some(host) = url.host_str() else { return String::new() };
    let scheme = url.scheme().to_string();
    let port = url.port().unwrap_or_else(|| default_port(&scheme));
    if !should_proxy_hostname(strip_brackets(host), port, env) {
        return String::new();
    }
    let mut proxy = proxy_env(&format!("{scheme}_proxy"), env);
    if proxy.is_empty() && matches!(scheme.as_str(), "http" | "https") {
        proxy = http_settings().proxy_setting().unwrap_or_default().to_string();
    }
    if proxy.is_empty() {
        proxy = proxy_env("all_proxy", env);
    }
    if !proxy.is_empty() && !proxy.contains("://") {
        proxy = format!("{scheme}://{proxy}");
    }
    proxy
}

/// The HTTP(S) proxy for `target`, if any. Errors for unparsable or SOCKS proxy URLs.
pub fn resolve_proxy_url(target: &str, env: &BTreeMap<String, String>) -> Result<Option<Url>, String> {
    let proxy = proxy_for_url(target, env);
    if proxy.is_empty() {
        return Ok(None);
    }
    let url = Url::parse(&proxy).map_err(|error| format!("Invalid proxy URL {}: {error}", serde_json::json!(proxy)))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("{UNSUPPORTED_PROXY_PROTOCOL_MESSAGE} Got {}:", url.scheme()));
    }
    Ok(Some(url))
}

/// `pi (<platform> <release>; <arch>)` with Node platform and architecture names.
pub fn pi_user_agent() -> String {
    let platform = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    };
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").map(|text| text.trim().to_string());
    match release {
        Ok(release) if !release.is_empty() => format!("pi ({platform} {release}; {arch})"),
        _ => format!("pi ({platform}; {arch})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(key, value)| (key.to_string(), value.to_string())).collect()
    }

    fn proxy(target: &str, pairs: &[(&str, &str)]) -> Option<String> {
        resolve_proxy_url(target, &env(pairs)).unwrap().map(|url| url.to_string())
    }

    #[test]
    fn scoped_env_selects_proxy_by_scheme() {
        let vars = [("HTTPS_PROXY", "http://secure:8443"), ("http_proxy", "plain:8080")];
        assert_eq!(proxy("https://api.example.com/v1", &vars).as_deref(), Some("http://secure:8443/"));
        assert_eq!(proxy("http://api.example.com/v1", &vars).as_deref(), Some("http://plain:8080/"));
        assert_eq!(proxy("https://x.dev", &[("ALL_PROXY", "http://all:1")]).as_deref(), Some("http://all:1/"));
    }

    #[test]
    fn no_proxy_matches_domains_ports_and_wildcards() {
        let base = [("https_proxy", "http://proxy:1")];
        let with = |no_proxy: &str| {
            let mut vars = base.to_vec();
            vars.push(("no_proxy", no_proxy));
            proxy("https://api.example.com:443/x", &vars)
        };
        assert_eq!(with("*"), None);
        assert_eq!(with("example.com"), None);
        assert_eq!(with(".example.com"), None);
        assert_eq!(with("*.example.com"), None);
        assert_eq!(with("api.example.com:443"), None);
        assert!(with("api.example.com:8443").is_some());
        assert!(with("other.com, notexample.com").is_some());
        assert_eq!(with("other.com example.com"), None);
        let ipv6 = [("http_proxy", "http://proxy:1"), ("no_proxy", "[::1]:8080")];
        assert_eq!(proxy("http://[::1]:8080/", &ipv6), None);
        assert!(proxy("http://[::1]:9090/", &ipv6).is_some());
    }

    #[test]
    fn socks_and_invalid_proxies_are_rejected() {
        let error = resolve_proxy_url("https://x.dev", &env(&[("https_proxy", "socks5://proxy:1080")])).unwrap_err();
        assert!(error.starts_with(UNSUPPORTED_PROXY_PROTOCOL_MESSAGE), "{error}");
        assert!(resolve_proxy_url("https://x.dev", &env(&[("https_proxy", "http://[bad")])).is_err());
    }

    #[test]
    fn idle_timeout_settings_parse_and_format() {
        assert_eq!(parse_http_idle_timeout_ms(&serde_json::json!("disabled")), Some(0));
        assert_eq!(parse_http_idle_timeout_ms(&serde_json::json!(" 1500.7 ")), Some(1500));
        assert_eq!(parse_http_idle_timeout_ms(&serde_json::json!(-1)), None);
        assert_eq!(parse_http_idle_timeout_ms(&serde_json::json!("")), None);
        assert_eq!(format_http_idle_timeout_ms(60_000), "1 min");
        assert_eq!(format_http_idle_timeout_ms(45_000), "45 sec");
        assert_eq!(HttpSettings::default().idle_timeout(), Some(Duration::from_millis(DEFAULT_HTTP_IDLE_TIMEOUT_MS)));
        assert_eq!(HttpSettings { idle_timeout_ms: Some(0), ..Default::default() }.idle_timeout(), None);
    }
}
