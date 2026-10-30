//! What the app around pi-ai supplies to ACP agents: the agents it knows, permission dialogs, the
//! map from pi sessions to ACP sessions, and the plan display.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock, RwLock};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How to start one ACP agent over stdio.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentLaunch {
    /// The pi provider ID of the agent, for example `kiro-acp`.
    pub provider: String,
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// One value of a selector, such as a model or a mode.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Choice {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A setting of the agent session: `modes` (set with `session/set_mode`) or a `configOptions` select.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Selector {
    pub id: String,
    pub name: String,
    /// `mode`, `model`, `thought_level`, or another `configOptions` category.
    pub category: String,
    pub current: Option<String>,
    pub choices: Vec<Choice>,
    /// True for the `modes` field of the session, which `session/set_mode` changes.
    pub legacy_mode: bool,
}

/// What an agent session offers, reported when it starts and when it changes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionInfo {
    pub provider: String,
    pub pi_session: String,
    pub acp_session: String,
    pub selectors: Vec<Selector>,
}

impl SessionInfo {
    /// The models of the agent: the `model` selector, or `models.availableModels` of older agents.
    pub fn models(&self) -> Vec<Choice> {
        self.selectors
            .iter()
            .find(|selector| selector.category == "model")
            .map(|s| s.choices.clone())
            .unwrap_or_default()
    }
}

/// A `session/request_permission` request.
#[derive(Clone, Debug, PartialEq)]
pub struct PermissionRequest {
    pub provider: String,
    pub agent_name: String,
    pub pi_session: String,
    /// The title of the tool call, or its kind when the agent sent no title.
    pub title: String,
    pub kind: Option<String>,
    /// `(optionId, name, kind)`; the kinds are `allow_once`, `allow_always`, `reject_once`, `reject_always`.
    pub options: Vec<(String, String, String)>,
    pub tool_call: Value,
}

/// A plan entry: `content`, `priority`, and `status` (`pending`, `in_progress`, `completed`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlanEntry {
    pub content: String,
    pub status: String,
}

#[async_trait]
pub trait AcpHost: Send + Sync {
    /// The chosen `optionId`, or `None` to answer `cancelled`.
    async fn request_permission(&self, request: PermissionRequest) -> Option<String>;
    fn session_changed(&self, _info: &SessionInfo) {}
    /// The ACP session that pi session `pi_session` used with `provider`, for `session/load`.
    fn recall_session(&self, _provider: &str, _pi_session: &str) -> Option<String> {
        None
    }
    fn plan(&self, _pi_session: &str, _entries: &[PlanEntry]) {}
    /// The working directory of `pi_session`; the process directory when `None`.
    fn session_cwd(&self, _pi_session: &str) -> Option<std::path::PathBuf> {
        None
    }
}

static AGENTS: LazyLock<RwLock<Vec<AgentLaunch>>> = LazyLock::new(RwLock::default);
static HOST: LazyLock<RwLock<Option<Arc<dyn AcpHost>>>> = LazyLock::new(RwLock::default);

/// Set the agents that the `acp` API can start, keyed by [`AgentLaunch::provider`].
pub fn set_agents(agents: Vec<AgentLaunch>) {
    *AGENTS.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = agents;
}

pub fn agent(provider: &str) -> Option<AgentLaunch> {
    AGENTS.read().unwrap_or_else(|poisoned| poisoned.into_inner()).iter().find(|a| a.provider == provider).cloned()
}

pub fn set_host(host: Option<Arc<dyn AcpHost>>) {
    *HOST.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = host;
}

pub(crate) fn host() -> Option<Arc<dyn AcpHost>> {
    HOST.read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
}
