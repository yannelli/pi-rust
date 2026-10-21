//! How requests identify the calling app to providers (upstream `utils/identity.ts`): the `User-Agent` and the
//! provider attribution headers. pi-ai sends a detailed `User-Agent` and no attribution unless asked to.

use reqwest::Url;
use serde::{Deserialize, Deserializer, Serialize};

use crate::types::Model;

/// The pi-ai version, the default version in the detailed `User-Agent`.
pub const PI_AI_VERSION: &str = env!("CARGO_PKG_VERSION");

const NVIDIA_NIM_HOST: &str = "integrate.api.nvidia.com";
const OPENCODE_HOST: &str = "opencode.ai";

/// The `User-Agent` sent to providers. JSON: `"detailed"`, `"short"`, or `{ "custom": "..." }`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "UserAgentWire", into = "UserAgentWire")]
pub enum UserAgentMode {
    /// `pi/<version> (<platform>; rust; <arch>)` with Node platform and architecture names.
    Detailed,
    /// `pi`.
    Short,
    /// The given value.
    Custom(String),
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum UserAgentWire {
    Named(UserAgentName),
    Custom { custom: String },
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum UserAgentName {
    Detailed,
    Short,
}

impl From<UserAgentWire> for UserAgentMode {
    fn from(wire: UserAgentWire) -> Self {
        match wire {
            UserAgentWire::Named(UserAgentName::Detailed) => Self::Detailed,
            UserAgentWire::Named(UserAgentName::Short) => Self::Short,
            UserAgentWire::Custom { custom } => Self::Custom(custom),
        }
    }
}

impl From<UserAgentMode> for UserAgentWire {
    fn from(mode: UserAgentMode) -> Self {
        match mode {
            UserAgentMode::Detailed => Self::Named(UserAgentName::Detailed),
            UserAgentMode::Short => Self::Named(UserAgentName::Short),
            UserAgentMode::Custom(custom) => Self::Custom { custom },
        }
    }
}

/// Who requests are attributed to. [`attribution_headers`] maps the fields to each provider's headers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientAttribution {
    /// App name: OpenRouter app title, NVIDIA invoke origin (first letter upper-cased), OpenCode client.
    pub name: String,
    /// App URL: OpenRouter `HTTP-Referer`, the app's identity in OpenRouter's rankings.
    pub url: String,
    /// App categories, such as `["cli-agent"]`: OpenRouter `X-OpenRouter-Categories`, comma-separated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub categories: Option<Vec<String>>,
}

/// pi's attribution (upstream `PI_ATTRIBUTION`), used by pi-agent, pi-durable, and the coding agent.
pub fn pi_attribution() -> ClientAttribution {
    ClientAttribution { name: "pi".into(), url: "https://pi.dev".into(), categories: Some(vec!["cli-agent".into()]) }
}

/// How pi-ai identifies the calling app to providers. Unset fields use pi-ai's defaults: a detailed `User-Agent`
/// with the pi-ai version and no attribution headers. Plain JSON, so it can be persisted.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientIdentity {
    /// Default: [`UserAgentMode::Detailed`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<UserAgentMode>,
    /// Provider attribution. `None` (absent) keeps the merged value; `Some(None)` (JSON `null`) turns it off.
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "present")]
    pub attribution: Option<Option<ClientAttribution>>,
    /// Version in the detailed `User-Agent`. Default: [`PI_AI_VERSION`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// A present field, `null` included, is `Some`.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(deserializer: D) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

impl ClientIdentity {
    /// An identity that attributes requests to `attribution`.
    pub fn attributed(attribution: ClientAttribution) -> Self {
        Self { attribution: Some(Some(attribution)), ..Self::default() }
    }
}

/// The provider and endpoint a request goes to.
#[derive(Clone, Copy, Debug)]
pub struct ClientIdentityTarget<'a> {
    pub provider: &'a str,
    pub base_url: &'a str,
}

impl<'a> From<&'a Model> for ClientIdentityTarget<'a> {
    fn from(model: &'a Model) -> Self {
        Self { provider: &model.provider, base_url: &model.base_url }
    }
}

/// Merge identities field by field; later sources win and unset fields are skipped. `None` without any source.
pub fn merge_client_identity<'a>(
    sources: impl IntoIterator<Item = Option<&'a ClientIdentity>>,
) -> Option<ClientIdentity> {
    let mut merged: Option<ClientIdentity> = None;
    for source in sources.into_iter().flatten() {
        let merged = merged.get_or_insert_with(ClientIdentity::default);
        if source.user_agent.is_some() {
            merged.user_agent = source.user_agent.clone();
        }
        if source.attribution.is_some() {
            merged.attribution = source.attribution.clone();
        }
        if source.version.is_some() {
            merged.version = source.version.clone();
        }
    }
    merged
}

/// The version in the detailed `User-Agent`.
pub fn client_version(identity: Option<&ClientIdentity>) -> &str {
    identity.and_then(|identity| identity.version.as_deref()).unwrap_or(PI_AI_VERSION)
}

fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

fn node_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    }
}

/// The `User-Agent` value of an identity.
pub fn format_user_agent(identity: Option<&ClientIdentity>) -> String {
    match identity.and_then(|identity| identity.user_agent.as_ref()).unwrap_or(&UserAgentMode::Detailed) {
        UserAgentMode::Custom(custom) => custom.clone(),
        UserAgentMode::Short => "pi".into(),
        UserAgentMode::Detailed => {
            format!("pi/{} ({}; rust; {})", client_version(identity), node_platform(), node_arch())
        }
    }
}

fn matches_host(base_url: &str, expected: &str) -> bool {
    Url::parse(base_url).ok().is_some_and(|url| url.host_str() == Some(expected))
}

fn is_openrouter(target: ClientIdentityTarget<'_>) -> bool {
    target.provider == "openrouter" || target.base_url.contains("openrouter.ai")
}

fn is_nvidia_nim(target: ClientIdentityTarget<'_>) -> bool {
    target.provider == "nvidia" || matches_host(target.base_url, NVIDIA_NIM_HOST)
}

fn is_opencode(target: ClientIdentityTarget<'_>) -> bool {
    matches!(target.provider, "opencode" | "opencode-go") || matches_host(target.base_url, OPENCODE_HOST)
}

fn capitalized(name: &str) -> String {
    let mut chars = name.chars();
    chars.next().map(|first| first.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

/// Provider attribution headers of an identity for a request target. Empty when attribution is off.
pub fn attribution_headers<'a>(
    target: impl Into<ClientIdentityTarget<'a>>,
    identity: Option<&ClientIdentity>,
) -> Vec<(&'static str, String)> {
    let target = target.into();
    let Some(ClientAttribution { name, url, categories }) =
        identity.and_then(|identity| identity.attribution.as_ref()).and_then(Option::as_ref)
    else {
        return Vec::new();
    };
    if is_openrouter(target) {
        let mut headers = vec![("HTTP-Referer", url.clone()), ("X-OpenRouter-Title", name.clone())];
        if let Some(categories) = categories.as_ref().filter(|categories| !categories.is_empty()) {
            headers.push(("X-OpenRouter-Categories", categories.join(",")));
        }
        return headers;
    }
    // NVIDIA expects a capitalized origin, such as `Pi`.
    if is_nvidia_nim(target) {
        return vec![("X-BILLING-INVOKE-ORIGIN", capitalized(name))];
    }
    if is_opencode(target) {
        return vec![("x-opencode-client", name.clone())];
    }
    Vec::new()
}

/// OpenCode's per-conversation routing header for requests to OpenCode, including custom providers pointing at it.
/// `None` without a session or for other targets.
pub fn opencode_session_header<'a>(
    target: impl Into<ClientIdentityTarget<'a>>,
    session_id: Option<&str>,
) -> Option<(&'static str, String)> {
    let session = session_id.filter(|session| !session.is_empty())?;
    is_opencode(target.into()).then(|| ("x-opencode-session", session.to_string()))
}

/// Default headers an identity adds to a request: `User-Agent` and provider attribution. Providers apply them
/// below model, provider, and request headers, so configured headers override them and suppressed request headers
/// remove them.
pub fn identity_headers<'a>(
    target: impl Into<ClientIdentityTarget<'a>>,
    identity: Option<&ClientIdentity>,
) -> Vec<(&'static str, String)> {
    let mut headers = vec![("User-Agent", format_user_agent(identity))];
    headers.extend(attribution_headers(target, identity));
    headers
}

#[cfg(test)]
#[path = "identity_tests.rs"]
mod tests;
