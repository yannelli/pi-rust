//! The live session of a pi session before and between prompts: opened from the current request context,
//! with modes and settings that the agent confirms before pi keeps them for later sessions of the slot.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use serde_json::json;

use super::runtime::{Runtime, SESSION_TIMEOUT, THOUGHT_LEVEL, rpc, selector, values};
use super::{Shape, effort_for, session_shape, with_levels};
use crate::providers::acp::{Selector, SessionInfo, session_selectors};
use crate::types::{Context, Model, ThinkingLevel};

const PROVIDER: &str = "google-antigravity";
const BUSY: &str = "Wait until the agent turn ends";

/// A mode or setting the agent confirmed; `legacy` names the session's `modes` (`session/set_mode`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Chosen {
    pub id: String,
    pub legacy: bool,
    pub value: String,
}

/// The runtime of a pi session and the confirmed choices that every new session of the slot gets.
#[derive(Default)]
pub(crate) struct Slot {
    pub runtime: Option<Runtime>,
    pub chosen: Vec<Chosen>,
}

pub(crate) type Shared = Arc<tokio::sync::Mutex<Slot>>;
static SLOTS: LazyLock<Mutex<HashMap<String, Shared>>> = LazyLock::new(Mutex::default);

/// The slot of a pi session, created when there is none.
pub(crate) fn slot(pi_session: &str) -> Shared {
    let mut slots = SLOTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    slots.entry(pi_session.to_string()).or_default().clone()
}

/// Whether `shared` is still the slot of `pi_session`; `close_session` removes it.
pub(crate) fn is_current(pi_session: &str, shared: &Shared) -> bool {
    let slots = SLOTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    slots.get(pi_session).is_some_and(|current| Arc::ptr_eq(current, shared))
}

fn existing(pi_session: &str) -> Option<Shared> {
    SLOTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get(pi_session).cloned()
}

pub(crate) fn closed(pi_session: &str) -> String {
    format!("The Antigravity session of {pi_session} was closed")
}

impl Runtime {
    /// The modes and `configOptions` selectors, as generic ACP agents report them.
    pub(super) fn selectors(&self) -> Vec<Selector> {
        let reply = self.modes.as_ref().map_or(serde_json::Value::Null, |modes| json!({"modes": modes}));
        session_selectors(&reply, Some(self.config.as_slice()))
    }

    fn info(&self, pi_session: &str) -> SessionInfo {
        let model = selector(&self.config, &["model"]).and_then(|option| option["currentValue"].as_str());
        SessionInfo {
            provider: PROVIDER.into(),
            pi_session: pi_session.into(),
            acp_session: self.session_id.clone(),
            selectors: self.selectors(),
            pi_model: model.unwrap_or_default().into(),
            images: self.capabilities.images,
            ..SessionInfo::default()
        }
    }

    /// Sets selector `id` to `value` unless it has it, and fails unless the agent then reports `value`.
    async fn confirm(&mut self, id: &str, legacy: bool, value: &str) -> Result<(), String> {
        let found = self.selectors().into_iter().find(|s| s.id == id && s.legacy_mode == legacy);
        let Some(selector) = found else { return Err(format!("Antigravity has no setting {id}")) };
        if selector.current.as_deref() == Some(value) {
            return Ok(());
        }
        if !selector.choices.iter().any(|choice| choice.id == value) {
            let offered: Vec<&str> = selector.choices.iter().map(|choice| choice.id.as_str()).collect();
            let name = &selector.name;
            return Err(format!("Antigravity does not offer {name} {value}; it offers: {}", offered.join(", ")));
        }
        let session = self.session_id.clone();
        if legacy {
            let params = json!({"sessionId": session, "modeId": value});
            self.connection.request("session/set_mode", params, SESSION_TIMEOUT).await.map_err(rpc)?;
            if let Some(modes) = self.modes.as_mut() {
                modes["currentModeId"] = json!(value);
            }
        } else {
            let params = json!({"sessionId": session, "configId": id, "value": value});
            let reply = self.connection.request("session/set_config_option", params, SESSION_TIMEOUT).await;
            if let Some(config) = reply.map_err(rpc)?["configOptions"].as_array() {
                self.config = config.clone();
            }
        }
        let now = self.selectors().into_iter().find(|s| s.id == id && s.legacy_mode == legacy);
        match now.and_then(|selector| selector.current) {
            Some(current) if current == value => Ok(()),
            current => {
                Err(format!("Antigravity kept {} at {}", selector.name, current.as_deref().unwrap_or("no value")))
            }
        }
    }

    /// Selects `model`, then `level` clamped to the thinking levels the agent offers for it now, and fails
    /// unless the agent reports both. A server without a model selector keeps its `<model>-<effort>` routing.
    pub(super) async fn select_confirmed(&mut self, model: &Model, level: ThinkingLevel) -> Result<(), String> {
        if selector(&self.config, &["model"]).is_none() {
            let effort = effort_for(model, level);
            self.select(&model.id, effort.as_deref()).await?;
            return self.confirm_selection(&model.id, effort.as_deref());
        }
        self.select(&model.id, None).await?;
        let offered = selector(&self.config, &THOUGHT_LEVEL).map(values).unwrap_or_default();
        let effort = effort_for(&with_levels(model.clone(), &offered), level);
        if let Some(effort) = effort.as_deref() {
            self.set_option(&THOUGHT_LEVEL, effort).await?;
        }
        self.confirm_selection(&model.id, effort.as_deref())
    }

    /// Fails unless the agent reports `model` and, when given, the `effort` thinking value.
    fn confirm_selection(&self, model: &str, effort: Option<&str>) -> Result<(), String> {
        let current = |kinds: &[&str]| {
            selector(&self.config, kinds).map(|option| option["currentValue"].as_str().unwrap_or_default().to_string())
        };
        if let Some(current) = current(&["model"]).filter(|current| current != model) {
            return Err(format!("Antigravity kept model {current}"));
        }
        match (effort, current(&THOUGHT_LEVEL)) {
            (Some(effort), Some(current)) if current != effort => {
                Err(format!("Antigravity kept thinking level {current}"))
            }
            _ => Ok(()),
        }
    }
}

/// Model and thinking come from pi each turn; the other choices are kept for new sessions.
fn kept(selector: &Selector) -> bool {
    selector.category != "model" && !THOUGHT_LEVEL.contains(&selector.category.as_str())
}

/// Reuses the slot's session when it was built for `shape` (and has seen at most `seen` messages), else
/// starts a new one and applies the confirmed choices to it. A running turn keeps its session.
pub(super) async fn ensure(slot: &mut Slot, shape: Shape, seen: Option<usize>) -> Result<(), String> {
    let stale = slot.runtime.as_ref().is_some_and(|runtime| {
        runtime.prompt.is_none()
            && (runtime.fingerprint != shape.fingerprint
                || seen.is_some_and(|seen| runtime.seen > seen)
                || runtime.connection.closed_reason().is_some())
    });
    if stale {
        if let Some(old) = slot.runtime.take() {
            old.shutdown().await;
        }
    }
    if slot.runtime.is_some() {
        return Ok(());
    }
    let cwd = std::env::current_dir().map_err(|error| format!("Cannot read the working directory: {error}"))?;
    let Shape { system, tools, fingerprint } = shape;
    let mut runtime = Runtime::start(cwd, system.as_deref(), tools, fingerprint).await?;
    for choice in slot.chosen.clone() {
        if let Err(error) = runtime.confirm(&choice.id, choice.legacy, &choice.value).await {
            runtime.shutdown().await;
            return Err(error);
        }
    }
    slot.runtime = Some(runtime);
    Ok(())
}

/// Opens (or reuses) the session of `pi_session` for `context`, the request of its next turn, and selects
/// `model` at `level`. The agent confirms both; no prompt is sent, so the first prompt reuses the session.
pub async fn open_session(
    pi_session: &str,
    context: &Context,
    model: &Model,
    level: ThinkingLevel,
) -> Result<SessionInfo, String> {
    let shared = slot(pi_session);
    let mut state = shared.lock().await;
    if !is_current(pi_session, &shared) {
        return Err(closed(pi_session));
    }
    if state.runtime.as_ref().is_some_and(|runtime| runtime.prompt.is_some()) {
        return Err(BUSY.into());
    }
    ensure(&mut state, session_shape(&crate::transcript::flatten_context(context)), None).await?;
    let runtime = state.runtime.as_mut().expect("ensured above");
    let selected = runtime.select_confirmed(model, level).await;
    if !is_current(pi_session, &shared) {
        if let Some(runtime) = state.runtime.take() {
            runtime.shutdown().await;
        }
        return Err(closed(pi_session));
    }
    selected?;
    Ok(state.runtime.as_ref().expect("ensured above").info(pi_session))
}

/// Sets selector `id` (the session's `modes` when `legacy`) of the open session to `value`. Only a value the
/// agent then reports is kept, and only modes and settings other than model and thinking are reapplied later.
pub async fn set_session_option(pi_session: &str, id: &str, legacy: bool, value: &str) -> Result<SessionInfo, String> {
    let shared = existing(pi_session).ok_or_else(|| format!("No Antigravity session is open for {pi_session}"))?;
    let mut state = shared.try_lock().map_err(|_| BUSY.to_string())?;
    if !is_current(pi_session, &shared) {
        return Err(closed(pi_session));
    }
    let runtime = state.runtime.as_mut().ok_or_else(|| format!("No Antigravity session is open for {pi_session}"))?;
    if runtime.prompt.is_some() {
        return Err(BUSY.into());
    }
    let confirmed = runtime.confirm(id, legacy, value).await;
    if !is_current(pi_session, &shared) {
        return Err(closed(pi_session));
    }
    confirmed?;
    let selector = runtime.selectors().into_iter().find(|s| s.id == id && s.legacy_mode == legacy);
    let info = runtime.info(pi_session);
    if selector.as_ref().is_some_and(kept) {
        state.chosen.retain(|choice| choice.id != id || choice.legacy != legacy);
        state.chosen.push(Chosen { id: id.into(), legacy, value: value.into() });
    }
    Ok(info)
}

/// What the open session of `pi_session` offers; `None` without one, or while a turn holds it.
pub fn session_info(pi_session: &str) -> Option<SessionInfo> {
    let shared = existing(pi_session)?;
    let state = shared.try_lock().ok()?;
    state.runtime.as_ref().map(|runtime| runtime.info(pi_session))
}

/// Ends the session of `pi_session`: an open or turn that holds it finishes first, then the agent is
/// cancelled, its server and tool bridge stop, and the confirmed choices are dropped with the slot.
pub async fn close_session(pi_session: &str) {
    let removed = SLOTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(pi_session);
    let Some(shared) = removed else { return };
    let mut state = shared.lock().await;
    state.chosen.clear();
    if let Some(runtime) = state.runtime.take() {
        if runtime.prompt.is_some() {
            runtime.connection.notify("session/cancel", json!({"sessionId": runtime.session_id}));
        }
        runtime.shutdown().await;
    }
}
