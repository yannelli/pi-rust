//! pi's side of the `acp` provider ([`pi_ai::providers::acp::AcpHost`]): permission dialogs, the
//! plan widget, the ACP session of each pi session, what each agent advertised, and `/undo`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, RwLock, Weak};

use async_trait::async_trait;
use pi_ai::providers::acp::{
    AcpHost, AgentCommand, Choice, FileChange, McpProxy, PermissionRequest, PlanEntry, PriorFile, SessionInfo,
};
use serde::{Deserialize, Serialize};

use super::workspace::{self, Snapshot};
use crate::agent_session::{AgentSession, ModelRuntime, Written};
use crate::extension_ui::{DialogOptions, ExtensionMode, ExtensionUi};
use crate::tools::PriorContent;

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
    /// The values of each provider's `thought_level` selector, for the pi thinking levels.
    #[serde(default)]
    pub thought_levels: BTreeMap<String, Vec<Choice>>,
    /// The slash commands each provider advertised in its last session.
    #[serde(default)]
    pub commands: BTreeMap<String, Vec<AgentCommand>>,
    /// The context window per provider and model ID, from `usage_update`.
    #[serde(default)]
    pub context: BTreeMap<String, BTreeMap<String, u64>>,
    /// Whether each provider accepts images.
    #[serde(default)]
    pub images: BTreeMap<String, bool>,
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

/// The workspace at the start of an ACP turn and the changes the agent reported since.
struct Turn {
    before: Snapshot,
    reported: BTreeMap<PathBuf, PriorContent>,
}

pub struct PiAcpHost {
    agent_dir: PathBuf,
    ui: RwLock<Option<(Arc<dyn ExtensionUi>, ExtensionMode)>>,
    runtime: RwLock<Weak<ModelRuntime>>,
    lock: Mutex<()>,
    turns: Mutex<HashMap<String, Turn>>,
}

static HOST: LazyLock<RwLock<Option<Arc<PiAcpHost>>>> = LazyLock::new(RwLock::default);
static SESSIONS: LazyLock<Mutex<Vec<Weak<AgentSession>>>> = LazyLock::new(Mutex::default);

/// Makes `session` known to ACP agents: its directory, its checkpoints, and that it is live.
pub fn track_session(session: &Arc<AgentSession>) {
    let mut sessions = SESSIONS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    sessions.retain(|weak| weak.strong_count() > 0);
    sessions.push(Arc::downgrade(session));
}

pub(crate) fn untrack_session(pi_session: &str) {
    let mut sessions = SESSIONS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    sessions.retain(|session| session.upgrade().is_some_and(|session| session.session_id() != pi_session));
}

/// The live session whose ID is `pi_session`.
fn session(pi_session: &str) -> Option<Arc<AgentSession>> {
    let sessions: Vec<Arc<AgentSession>> = {
        let sessions = SESSIONS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        sessions.iter().filter_map(Weak::upgrade).collect()
    };
    sessions.into_iter().find(|session| session.session_id() == pi_session)
}

fn prior_content(prior: PriorFile) -> PriorContent {
    match prior {
        PriorFile::Bytes(bytes) => PriorContent::Bytes(bytes),
        PriorFile::Missing => PriorContent::Missing,
        PriorFile::NotRestorable(reason) => PriorContent::NotRestorable(reason),
    }
}

/// The commands that `provider` advertised in its last session.
pub fn commands(agent_dir: &Path, provider: &str) -> Vec<AgentCommand> {
    read_state(agent_dir).commands.remove(provider).unwrap_or_default()
}

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
                turns: Mutex::default(),
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
        let thought = info.selectors.iter().find(|selector| selector.category == "thought_level");
        let (thinking, levels) = (thought.is_some(), thought.map(|selector| selector.choices.clone()));
        let agent_model = info.selectors.iter().find(|s| s.category == "model").and_then(|s| s.current.clone());
        let mut models_changed = false;
        self.update(|state| {
            let provider = &info.provider;
            let key = session_key(provider, &info.pi_session);
            let mut changed = state.sessions.insert(key, info.acp_session.clone()).as_ref() != Some(&info.acp_session);
            if !models.is_empty() && state.models.get(provider) != Some(&models) {
                state.models.insert(provider.clone(), models.clone());
                models_changed = true;
            }
            if state.thinking.get(provider) != Some(&thinking) {
                state.thinking.insert(provider.clone(), thinking);
                models_changed = true;
            }
            let stored = match &levels {
                Some(levels) => state.thought_levels.insert(provider.clone(), levels.clone()),
                None => state.thought_levels.remove(provider),
            };
            models_changed |= stored != levels;
            if state.images.insert(provider.clone(), info.images) != Some(info.images) {
                models_changed = true;
            }
            if let Some(size) = info.context_window {
                let windows = state.context.entry(provider.clone()).or_default();
                for model in std::iter::once(info.pi_model.clone()).chain(agent_model) {
                    models_changed |= windows.insert(model, size) != Some(size);
                }
            }
            let stored = state.commands.get(provider).map_or(&[][..], Vec::as_slice);
            if info.commands_known && stored != info.commands.as_slice() {
                match info.commands.is_empty() {
                    true => state.commands.remove(provider),
                    false => state.commands.insert(provider.clone(), info.commands.clone()),
                };
                changed = true;
            }
            changed |= models_changed;
            changed
        });
        let runtime = self.runtime.read().unwrap_or_else(|poisoned| poisoned.into_inner()).upgrade();
        if let (true, Some(runtime)) = (models_changed, runtime) {
            super::register::refresh_models(&runtime, &self.agent_dir, &info.provider);
        }
    }

    fn session_cwd(&self, pi_session: &str) -> Option<PathBuf> {
        session(pi_session).map(|session| session.cwd().to_path_buf())
    }

    fn live_session(&self, pi_session: &str) -> bool {
        session(pi_session).is_some()
    }

    fn file_changed(&self, pi_session: &str, change: FileChange) {
        let mut turns = self.turns.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(turn) = turns.get_mut(pi_session) {
            turn.reported.entry(change.path).or_insert_with(|| prior_content(change.prior));
            return;
        }
        drop(turns);
        if let Some(session) = session(pi_session) {
            let written = change.written.as_deref().map_or(Written::Deleted, Written::Bytes);
            session.record_file_change(&change.path, prior_content(change.prior), written, false);
        }
    }

    fn turn_started(&self, pi_session: &str, cwd: &Path) {
        let turn = Turn { before: workspace::take(cwd), reported: BTreeMap::new() };
        self.turns.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(pi_session.to_string(), turn);
    }

    /// Records what changed under `cwd` since the turn started. The snapshot gives the prior
    /// state; changes the agent did not report are marked as detected.
    fn turn_ended(&self, pi_session: &str, cwd: &Path) {
        let turn = self.turns.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(pi_session);
        let (Some(Turn { before, mut reported }), Some(session)) = (turn, session(pi_session)) else { return };
        let root = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        for difference in before.differences(&root) {
            let detected = reported.remove(&difference.path).is_none();
            let written = if difference.exists { Written::OnDisk } else { Written::Deleted };
            session.record_file_change(&difference.path, difference.prior, written, detected);
        }
        for (path, prior) in reported.into_iter().filter(|(path, _)| !before.covers(&root, path)) {
            let written = if path.exists() { Written::OnDisk } else { Written::Deleted };
            session.record_file_change(&path, prior, written, false);
        }
    }

    async fn mcp_proxy(&self, pi_session: &str) -> McpProxy {
        let manager = session(pi_session).and_then(|session| session.mcp_manager());
        match manager {
            Some(manager) => super::mcp_proxy::offer(&manager).await,
            None => McpProxy::default(),
        }
    }

    async fn call_mcp_proxy(&self, pi_session: &str, tool: &str, arguments: serde_json::Value) -> serde_json::Value {
        let manager = session(pi_session).and_then(|session| session.mcp_manager());
        match manager {
            Some(manager) => super::mcp_proxy::call(&manager, tool, arguments).await,
            None => {
                serde_json::json!({"content": [{"type": "text", "text": "The pi session has ended"}], "isError": true})
            }
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
