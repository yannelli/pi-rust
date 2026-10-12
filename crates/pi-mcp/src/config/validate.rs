//! Validation of `mcpServers` entries, with the upstream error messages.

use serde_json::{Map, Value};
use url::Url;

use super::{McpExposure, McpServerConfig};

const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];
const EXPOSURES: &str = "\"codemode\", \"deferred\", \"direct\", \"hidden\"";

fn is_server_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|character| character.is_ascii_alphanumeric() || character == '_' || character == '-')
}

fn is_string_record(value: &Value) -> bool {
    value.as_object().is_some_and(|object| object.values().all(Value::is_string))
}

fn is_exposure(value: &Value) -> bool {
    value.as_str().and_then(McpExposure::parse).is_some()
}

fn host(url: &Url) -> String {
    match url.host() {
        Some(url::Host::Ipv6(address)) => format!("[{address}]"),
        Some(host) => host.to_string(),
        None => String::new(),
    }
}

/// Whether a redirect URI can be served by the loopback callback server.
pub fn is_loopback_redirect_uri(value: &str) -> bool {
    let Ok(url) = Url::parse(value) else { return false };
    url.scheme() == "http"
        && LOOPBACK_HOSTS.contains(&host(&url).as_str())
        && url.query().is_none_or(str::is_empty)
        && url.fragment().is_none_or(str::is_empty)
}

/// A copy with `codemode-deferred`, the old name of `codemode`, replaced.
pub fn resolve_exposure_aliases(value: &Map<String, Value>) -> Map<String, Value> {
    let resolve = |value: &Value| if value == "codemode-deferred" { Value::from("codemode") } else { value.clone() };
    let mut resolved = value.clone();
    if let Some(exposure) = value.get("exposure") {
        resolved.insert("exposure".into(), resolve(exposure));
    }
    if let Some(Value::Object(tools)) = value.get("toolExposure") {
        let tools = tools.iter().map(|(tool, exposure)| (tool.clone(), resolve(exposure))).collect();
        resolved.insert("toolExposure".into(), Value::Object(tools));
    }
    resolved
}

fn validate_oauth(value: Option<&Value>) -> Option<String> {
    let value = value?;
    let Some(oauth) = value.as_object() else { return Some("oauth must be an object".into()) };
    let not_string = |key: &str| oauth.get(key).is_some_and(|value| !value.is_string());
    if not_string("clientId") {
        return Some("oauth.clientId must be a string".into());
    }
    if not_string("clientSecret") {
        return Some("oauth.clientSecret must be a string".into());
    }
    let port = oauth.get("callbackPort");
    if let Some(port) = port
        && !port.as_u64().is_some_and(|port| (1..=65535).contains(&port))
    {
        return Some("oauth.callbackPort must be a port number".into());
    }
    if let Some(callback) = oauth.get("callbackUrl") {
        let Some(callback) = callback.as_str().filter(|callback| is_loopback_redirect_uri(callback)) else {
            return Some(
                "oauth.callbackUrl must be an http URI on localhost, 127.0.0.1, or [::1] without query or fragment"
                    .into(),
            );
        };
        let url_port = Url::parse(callback).ok().and_then(|url| url.port());
        if let (Some(url_port), Some(port)) = (url_port, port.and_then(Value::as_u64))
            && u64::from(url_port) != port
        {
            return Some("oauth.callbackUrl and oauth.callbackPort name different ports".into());
        }
    }
    if not_string("scope") {
        return Some("oauth.scope must be a string".into());
    }
    if let Some(name) = oauth.get("clientName")
        && !name.as_str().is_some_and(|name| !name.trim().is_empty())
    {
        return Some("oauth.clientName must be a non-empty string".into());
    }
    if let Some(registration) = oauth.get("clientRegistration").filter(|registration| *registration != "dcr") {
        if registration != "cimd" {
            return Some("oauth.clientRegistration must be \"dcr\" or \"cimd\"".into());
        }
        if oauth.contains_key("clientId") || oauth.contains_key("clientName") {
            return Some(
                "oauth.clientRegistration \"cimd\" cannot be combined with oauth.clientId or oauth.clientName".into(),
            );
        }
        let callback = oauth.get("callbackUrl").and_then(Value::as_str).and_then(|callback| Url::parse(callback).ok());
        if callback.is_some_and(|callback| host(&callback) == "[::1]" || callback.path() != "/callback") {
            return Some(
                "oauth.clientRegistration \"cimd\" requires oauth.callbackUrl on localhost or 127.0.0.1 with path /callback"
                    .into(),
            );
        }
    }
    if let Some(metadata) = oauth.get("authServerMetadataUrl") {
        let url = metadata.as_str().and_then(|url| Url::parse(url).ok());
        let allowed = url.is_some_and(|url| {
            url.scheme() == "https" || (url.scheme() == "http" && LOOPBACK_HOSTS.contains(&host(&url).as_str()))
        });
        if !allowed {
            return Some(
                "oauth.authServerMetadataUrl must be an https URL, or http on localhost, 127.0.0.1, or [::1]".into(),
            );
        }
    }
    None
}

fn validate_http(name: &str, value: &Map<String, Value>, url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok().filter(|url| url.scheme() == "http" || url.scheme() == "https");
    let Some(parsed) = parsed else { return Some(format!("server \"{name}\": url must be an http or https URL")) };
    if value.get("headers").is_some_and(|headers| !is_string_record(headers)) {
        return Some(format!("server \"{name}\": headers must map names to strings"));
    }
    if let Some(error) = validate_oauth(value.get("oauth")) {
        return Some(format!("server \"{name}\": {error}"));
    }
    if let Some(auth) = value.get("auth") {
        if !auth.get("provider").and_then(Value::as_str).is_some_and(|provider| !provider.is_empty()) {
            return Some(format!("server \"{name}\": auth.provider must be a provider name"));
        }
        if parsed.scheme() != "https" && !LOOPBACK_HOSTS.contains(&host(&parsed).as_str()) {
            return Some(format!(
                "server \"{name}\": auth requires an https URL, or http on localhost, 127.0.0.1, or [::1]"
            ));
        }
    }
    None
}

fn validate_stdio(name: &str, value: &Map<String, Value>) -> Option<String> {
    let args = value.get("args");
    if args.is_some_and(|args| !args.as_array().is_some_and(|args| args.iter().all(Value::is_string))) {
        return Some(format!("server \"{name}\": args must be an array of strings"));
    }
    if value.get("env").is_some_and(|env| !is_string_record(env)) {
        return Some(format!("server \"{name}\": env must map names to strings"));
    }
    if value.get("cwd").is_some_and(|cwd| !cwd.is_string()) {
        return Some(format!("server \"{name}\": cwd must be a string"));
    }
    None
}

/// Validate one server entry. Returns the config with exposure aliases resolved, or an error message.
pub fn validate_mcp_server_config(name: &str, raw: &Value) -> Result<McpServerConfig, String> {
    if !is_server_name(name) {
        return Err(format!("invalid server name \"{name}\" (use letters, digits, \"_\" and \"-\")"));
    }
    let Some(raw) = raw.as_object() else { return Err(format!("server \"{name}\" must be an object")) };
    let value = resolve_exposure_aliases(raw);
    if value.get("exposure").is_some_and(|exposure| !is_exposure(exposure)) {
        return Err(format!("server \"{name}\": exposure must be one of {EXPOSURES}"));
    }
    if let Some(tools) = value.get("toolExposure") {
        let Some(tools) = tools.as_object() else {
            return Err(format!("server \"{name}\": toolExposure must map tool names to exposures"));
        };
        if let Some((tool, _)) = tools.iter().find(|(_, exposure)| !is_exposure(exposure)) {
            return Err(format!("server \"{name}\": toolExposure \"{tool}\" must be one of {EXPOSURES}"));
        }
    }
    if value.get("enabled").is_some_and(|enabled| !enabled.is_boolean()) {
        return Err(format!("server \"{name}\": enabled must be a boolean"));
    }
    if value.get("description").is_some_and(|description| !description.is_string()) {
        return Err(format!("server \"{name}\": description must be a string"));
    }
    if value.get("timeout").is_some_and(|timeout| !timeout.as_f64().is_some_and(|timeout| timeout > 0.0)) {
        return Err(format!("server \"{name}\": timeout must be a positive number of seconds"));
    }
    let kind = value.get("type");
    if kind.is_some_and(|kind| kind == "sse") {
        return Err(format!("server \"{name}\": legacy SSE transport is not supported; use the streamable HTTP URL"));
    }
    let kind_is = |allowed: &[&str]| kind.is_none_or(|kind| allowed.iter().any(|allowed| kind == *allowed));
    let error =
        if let Some(url) = value.get("url").and_then(Value::as_str).filter(|_| kind_is(&["http", "streamable-http"])) {
            validate_http(name, &value, url)
        } else if value.get("command").is_some_and(Value::is_string) && kind_is(&["stdio"]) {
            validate_stdio(name, &value)
        } else {
            Some(format!("server \"{name}\" needs either \"command\" (stdio) or \"url\" (streamable HTTP)"))
        };
    if let Some(error) = error {
        return Err(error);
    }
    serde_json::from_value(Value::Object(value)).map_err(|error| format!("server \"{name}\": {error}"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn checks_loopback_redirect_uris() {
        assert!(is_loopback_redirect_uri("http://localhost:8080/callback"));
        assert!(is_loopback_redirect_uri("http://[::1]/cb"));
        assert!(!is_loopback_redirect_uri("https://localhost/callback"));
        assert!(!is_loopback_redirect_uri("http://127.0.0.1/cb?x=1"));
        assert!(!is_loopback_redirect_uri("http://example.com/cb"));
    }

    #[test]
    fn rejects_entries_with_upstream_messages() {
        let error = |name: &str, raw: Value| validate_mcp_server_config(name, &raw).unwrap_err();
        assert_eq!(
            error("x", json!({ "command": "a", "timeout": 0 })),
            "server \"x\": timeout must be a positive number of seconds"
        );
        assert_eq!(
            error("x", json!({ "command": "a", "args": [1] })),
            "server \"x\": args must be an array of strings"
        );
        assert_eq!(
            error("x", json!({ "command": "a", "env": { "A": 1 } })),
            "server \"x\": env must map names to strings"
        );
        assert_eq!(
            error("x", json!({ "url": "https://a", "headers": [] })),
            "server \"x\": headers must map names to strings"
        );
        assert_eq!(error("x", json!([])), "server \"x\" must be an object");
        assert_eq!(
            error("x", json!({ "type": "stdio", "url": "https://a" })),
            "server \"x\" needs either \"command\" (stdio) or \"url\" (streamable HTTP)"
        );
        let config = validate_mcp_server_config(
            "x",
            &json!({ "type": "streamable-http", "url": "https://a/mcp", "timeout": 5 }),
        )
        .unwrap();
        assert_eq!((config.url.as_deref(), config.timeout), (Some("https://a/mcp"), Some(5.0)));
    }
}
