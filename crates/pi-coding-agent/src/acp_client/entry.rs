//! One `acpAgents` settings entry: how pi starts an ACP agent.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpAgentEntry {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default = "enabled_default")]
    pub enabled: bool,
    /// `registry`, `builtin`, or `custom`.
    #[serde(default = "source_default")]
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_dir: Option<String>,
    /// A login program for agents that do not start before a login, such as `kiro-cli login`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_command: Option<Vec<String>>,
}

fn enabled_default() -> bool {
    true
}

fn source_default() -> String {
    "custom".into()
}

/// The settings file that holds an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AcpScope {
    Global,
    Project,
}

impl AcpScope {
    pub fn label(self) -> &'static str {
        match self {
            AcpScope::Global => "global",
            AcpScope::Project => "project",
        }
    }
}

/// The pi provider ID of an agent: the agent ID when it ends in `-acp`, else `<id>-acp`.
pub fn provider_id(agent_id: &str) -> String {
    if agent_id.ends_with("-acp") { agent_id.to_string() } else { format!("{agent_id}-acp") }
}
