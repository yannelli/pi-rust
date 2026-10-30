//! The ACP registry index (`agentclientprotocol/registry`) and the built-in Kiro and Amp entries.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const REGISTRY_URL: &str = "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RegistryAgent {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub repository: Option<String>,
    pub website: Option<String>,
    pub authors: Vec<String>,
    pub license: Option<String>,
    pub icon: Option<String>,
    pub distribution: Distribution,
    /// Set for the entries of [`builtin_agents`].
    #[serde(skip)]
    pub builtin: bool,
    /// A program that the user installs, started from PATH. Set for Kiro.
    #[serde(skip)]
    pub local: Option<LocalCommand>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Distribution {
    pub binary: BTreeMap<String, BinaryTarget>,
    pub npx: Option<PackageTarget>,
    pub uvx: Option<PackageTarget>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BinaryTarget {
    pub archive: String,
    pub sha256: Option<String>,
    /// Relative to the unpacked archive.
    pub cmd: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PackageTarget {
    pub package: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalCommand {
    pub program: String,
    pub args: Vec<String>,
    /// Shown when `program` is not on PATH.
    pub install_hint: String,
    pub login: Vec<String>,
}

/// [`REGISTRY_URL`], or `PI_ACP_REGISTRY_URL` when set.
pub fn registry_url() -> String {
    std::env::var("PI_ACP_REGISTRY_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
        .unwrap_or_else(|| REGISTRY_URL.into())
}

pub async fn fetch(url: &str) -> Result<Vec<RegistryAgent>, String> {
    fetch_with(&pi_ai::http::shared_client(), url).await
}

/// Agents of the index at `url`. Agents that do not parse are skipped.
pub(crate) async fn fetch_with(client: &reqwest::Client, url: &str) -> Result<Vec<RegistryAgent>, String> {
    let response = client
        .get(url)
        .header(reqwest::header::USER_AGENT, crate::platform::pi_user_agent(crate::platform::VERSION))
        .send()
        .await
        .map_err(|error| format!("Could not fetch the ACP registry {url}: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("Could not fetch the ACP registry {url}: HTTP {}", status.as_u16()));
    }
    let bytes = response.bytes().await.map_err(|error| format!("Could not fetch the ACP registry {url}: {error}"))?;
    let body: Value =
        serde_json::from_slice(&bytes).map_err(|error| format!("The ACP registry {url} is not valid JSON: {error}"))?;
    let Some(Value::Array(agents)) = body.get("agents") else {
        return Err(format!("The ACP registry {url} has no agents list"));
    };
    Ok(agents
        .iter()
        .filter_map(|agent| serde_json::from_value::<RegistryAgent>(agent.clone()).ok())
        .filter(|agent| !agent.id.is_empty())
        .collect())
}

pub fn builtin_agents() -> Vec<RegistryAgent> {
    let kiro = RegistryAgent {
        id: "kiro".into(),
        name: "Kiro CLI (V3)".into(),
        description: "Kiro CLI V3 over ACP (kiro-cli acp --agent-engine=v3)".into(),
        website: Some("https://kiro.dev/docs/cli/v3/".into()),
        builtin: true,
        local: Some(LocalCommand {
            program: "kiro-cli".into(),
            args: vec!["acp".into(), "--agent-engine=v3".into()],
            install_hint: "Install Kiro CLI with `curl -fsSL https://cli.kiro.dev/install | bash`, then try again."
                .into(),
            login: vec!["kiro-cli".into(), "login".into()],
        }),
        ..RegistryAgent::default()
    };
    let amp = RegistryAgent {
        id: "amp-acp".into(),
        name: "Amp".into(),
        description: "Amp through the amp-acp adapter (npx amp-acp)".into(),
        repository: Some("https://github.com/tao12345666333/amp-acp".into()),
        website: Some("https://ampcode.com".into()),
        distribution: Distribution {
            npx: Some(PackageTarget { package: "amp-acp".into(), ..PackageTarget::default() }),
            ..Distribution::default()
        },
        builtin: true,
        ..RegistryAgent::default()
    };
    vec![kiro, amp]
}

/// `agents` plus the built-in entries whose ID the registry does not list.
pub fn with_builtins(mut agents: Vec<RegistryAgent>) -> Vec<RegistryAgent> {
    for builtin in builtin_agents() {
        if !agents.iter().any(|agent| agent.id == builtin.id) {
            agents.push(builtin);
        }
    }
    agents
}

/// The registry platform key of this build, such as `darwin-aarch64`.
pub fn platform_key() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("darwin-aarch64"),
        ("macos", "x86_64") => Some("darwin-x86_64"),
        ("linux", "aarch64") => Some("linux-aarch64"),
        ("linux", "x86_64") => Some("linux-x86_64"),
        ("windows", "aarch64") => Some("windows-aarch64"),
        ("windows", "x86_64") => Some("windows-x86_64"),
        _ => None,
    }
}
