//! pi's side of the `acp` provider ([`pi_ai::providers::acp::AcpHost`]): permission dialogs, the
//! plan widget, the ACP session of each pi session, and the models each agent advertised.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, RwLock, Weak};

use async_trait::async_trait;
use pi_ai::providers::acp::{AcpHost, Choice, PermissionRequest, PlanEntry, SessionInfo};
use serde::{Deserialize, Serialize};

use crate::agent_session::ModelRuntime;
use crate::extension_ui::{DialogOptions, ExtensionMode, ExtensionUi};

const PLAN_WIDGET: &str = "acp-plan";

/// `<agent_dir>/acp/state.json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostState {
    /// `"<provider> <pi session>"` to the ACP session ID.
    #[serde(default)]
    pub sessions: BTreeMap<String, String>,
    /// The models each provider advertised in its last session.
    #[serde(default)]
    pub models: BTreeMap<String, Vec<Choice>>,
    /// Providers whose agent has a `thought_level` selector.
    #[serde(default)]
    pub thinking: BTreeMap<String, bool>,
}

pub fn state_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("acp").join("state.json")
}

pub fn read_state(agent_dir: &Path) -> HostState {
    let text = std::fs::read_to_string(state_path(agent_dir)).unwrap_or_default();
    serde_json::from_str(&text).unwrap_or_default()
}

fn write_state(agent_dir: &Path, state: &HostState) {
    let path = state_path(agent_dir);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(state) {
        let _ = std::fs::write(path, text);
    }
}

pub struct PiAcpHost {
    agent_dir: PathBuf,
    ui: RwLock<Option<(Arc<dyn ExtensionUi>, ExtensionMode)>>,
    runtime: RwLock<Weak<ModelRuntime>>,
    lock: Mutex<()>,
}

static HOST: LazyLock<RwLock<Option<Arc<PiAcpHost>>>> = LazyLock::new(RwLock::default);

/// The host for `agent_dir`, installed in pi-ai on first use.
pub fn install(agent_dir: &Path, runtime: &Arc<ModelRuntime>) -> Arc<PiAcpHost> {
    let mut slot = HOST.write().unwrap_or_else(|poisoned| poisoned.into_inner());
    let host = match slot.as_ref().filter(|host| host.agent_dir == agent_dir) {
        Some(host) => host.clone(),
        None => {
            let host = Arc::new(PiAcpHost {
                agent_dir: agent_dir.to_path_buf(),
                ui: RwLock::default(),
                runtime: RwLock::default(),
                lock: Mutex::default(),
            });
            pi_ai::providers::acp::set_host(Some(host.clone()));
            *slot = Some(host.clone());
            host
        }
    };
    *host.runtime.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::downgrade(runtime);
    host
}

/// Where permission dialogs and the plan widget go: the UI that the mode bound last.
pub fn set_ui(ui: Arc<dyn ExtensionUi>, mode: ExtensionMode) {
    if let Some(host) = HOST.read().unwrap_or_else(|poisoned| poisoned.into_inner()).as_ref() {
        *host.ui.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((ui, mode));
    }
}

fn session_key(provider: &str, pi_session: &str) -> String {
    format!("{provider} {pi_session}")
}

/// Unique labels for the permission options, in their order.
fn labels(request: &PermissionRequest) -> Vec<String> {
    let mut labels: Vec<String> = Vec::new();
    for (_, name, kind) in &request.options {
        let base = if name.is_empty() { kind.replace('_', " ") } else { name.clone() };
        let label = if labels.contains(&base) { format!("{base} ({kind})") } else { base };
        labels.push(label);
    }
    labels
}

impl PiAcpHost {
    fn ui(&self) -> Option<Arc<dyn ExtensionUi>> {
        let ui = self.ui.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        ui.as_ref().filter(|(_, mode)| mode.has_ui()).map(|(ui, _)| ui.clone())
    }

    fn update(&self, change: impl FnOnce(&mut HostState) -> bool) -> HostState {
        let _guard = self.lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut state = read_state(&self.agent_dir);
        if change(&mut state) {
            write_state(&self.agent_dir, &state);
        }
        state
    }
}

#[async_trait]
impl AcpHost for PiAcpHost {
    async fn request_permission(&self, request: PermissionRequest) -> Option<String> {
        let Some(ui) = self.ui() else {
            let reject = request.options.iter().find(|(_, _, kind)| kind == "reject_once");
            return reject.map(|(id, _, _)| id.clone());
        };
        let labels = labels(&request);
        let title = format!("{} asks to run: {}", request.agent_name, request.title);
        let chosen = ui.select(&title, &labels, DialogOptions::default()).await?;
        let index = labels.iter().position(|label| *label == chosen)?;
        request.options.get(index).map(|(id, _, _)| id.clone())
    }

    fn session_changed(&self, info: &SessionInfo) {
        let models = info.models();
        let thinking = info.selectors.iter().any(|selector| selector.category == "thought_level");
        let mut models_changed = false;
        self.update(|state| {
            let key = session_key(&info.provider, &info.pi_session);
            let mut changed = state.sessions.insert(key, info.acp_session.clone()).as_ref() != Some(&info.acp_session);
            if !models.is_empty() && state.models.get(&info.provider) != Some(&models) {
                state.models.insert(info.provider.clone(), models.clone());
                models_changed = true;
            }
            if state.thinking.get(&info.provider) != Some(&thinking) {
                state.thinking.insert(info.provider.clone(), thinking);
                models_changed = true;
            }
            changed |= models_changed;
            changed
        });
        let runtime = self.runtime.read().unwrap_or_else(|poisoned| poisoned.into_inner()).upgrade();
        if let (true, Some(runtime)) = (models_changed, runtime) {
            super::register::refresh_models(&runtime, &self.agent_dir, &info.provider);
        }
    }

    fn recall_session(&self, provider: &str, pi_session: &str) -> Option<String> {
        read_state(&self.agent_dir).sessions.get(&session_key(provider, pi_session)).cloned()
    }

    fn plan(&self, _pi_session: &str, entries: &[PlanEntry]) {
        let Some(ui) = self.ui() else { return };
        if entries.iter().all(|entry| entry.status == "completed") {
            return ui.set_widget(PLAN_WIDGET, None, None);
        }
        let lines: Vec<String> = entries
            .iter()
            .map(|entry| {
                let mark = match entry.status.as_str() {
                    "completed" => "[x]",
                    "in_progress" => "[~]",
                    _ => "[ ]",
                };
                format!("{mark} {}", entry.content)
            })
            .collect();
        ui.set_widget(PLAN_WIDGET, Some(&lines), None);
    }
}
