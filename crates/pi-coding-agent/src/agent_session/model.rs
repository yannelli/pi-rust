//! Model and thinking level changes (upstream `setModel`, `cycleModel`, `setThinkingLevel`,
//! `cycleThinkingLevel`), each recorded in the session.

use pi_agent::QueueMode;
use pi_ai::models::{clamp_thinking_level, models_are_equal, supported_thinking_levels};
use pi_ai::simple_options::supports_fast;
use pi_ai::{Model, ThinkingLevel};

use super::{AgentSession, AgentSessionEvent, lock};
use crate::model_resolver::ScopedModel;
use crate::settings::DEFAULT_THINKING_LEVEL;

/// `model_select` for extensions, unless the model did not change.
fn model_select_event(model: &Model, previous: Option<&Model>, source: &str) -> Option<serde_json::Value> {
    if previous.is_some_and(|previous| models_are_equal(previous, model)) {
        return None;
    }
    Some(serde_json::json!({"type": "model_select", "model": model, "previousModel": previous, "source": source}))
}

/// Result of [`AgentSession::cycle_model`].
#[derive(Clone, Debug, PartialEq)]
pub struct ModelCycleResult {
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    /// Whether the cycle went through the scoped models (`--models`) or all available ones.
    pub is_scoped: bool,
}

impl AgentSession {
    /// Refresh ACP model capabilities and its advertised current thinking level without changing the model selection.
    pub fn refresh_acp_model_metadata(&self, info: &pi_ai::providers::acp::SessionInfo) -> bool {
        if let Some(current) = self.model().filter(|model| model.api == pi_ai::providers::antigravity_acp::API) {
            return self.refresh_antigravity_thinking(&current, info);
        }
        let Some(current) = self.model().filter(|model| model.api == pi_ai::providers::acp::API) else {
            return false;
        };
        if current.provider != info.provider || self.session_id() != info.pi_session {
            return false;
        }
        let Some(model) = self.model_runtime().get_model(&current.provider, &current.id) else {
            return false;
        };
        let remote = info
            .selectors
            .iter()
            .find(|selector| selector.category == "thought_level")
            .and_then(|selector| selector.current.as_deref());
        let level: Option<ThinkingLevel> = model.thinking_level_map.as_ref().and_then(|map| {
            map.iter().find_map(|(level, value)| match value.as_deref() {
                Some(value) if Some(value) == remote => level.parse().ok(),
                _ => None,
            })
        });
        let mut refreshed = false;
        self.agent.update_state(|state| {
            if models_are_equal(&state.model, &current) {
                state.model = model.clone();
                refreshed = true;
            }
        });
        if let Some(level) = level.filter(|_| refreshed) {
            self.set_thinking_level(level, false);
        }
        refreshed
    }

    /// Antigravity's catalog model with the thinking levels its open session offers now; the session's model
    /// changes, the catalog stays.
    fn refresh_antigravity_thinking(&self, current: &Model, info: &pi_ai::providers::acp::SessionInfo) -> bool {
        if self.session_id() != info.pi_session || current.id != info.pi_model {
            return false;
        }
        let live = pi_ai::providers::antigravity_acp::live_model(current, info).unwrap_or_else(|| current.clone());
        let remote = info
            .selectors
            .iter()
            .find(|selector| selector.category == "thought_level")
            .and_then(|selector| selector.current.as_deref());
        let level: Option<ThinkingLevel> = live.thinking_level_map.as_ref().and_then(|map| {
            map.iter().find_map(|(level, value)| match value.as_deref() {
                Some(value) if Some(value) == remote => level.parse().ok(),
                _ => None,
            })
        });
        let mut refreshed = false;
        self.agent.update_state(|state| {
            if models_are_equal(&state.model, current) {
                state.model = live.clone();
                refreshed = true;
            }
        });
        if let Some(level) = level.filter(|_| refreshed) {
            self.set_thinking_level(level, false);
        }
        refreshed
    }

    /// Switch to `model`. The thinking level becomes the model's `modelThinkingLevels` entry, the
    /// `defaultThinkingLevel` setting, or the current level. `persist` saves the default model.
    pub async fn set_model(&self, model: Model, persist: bool) -> anyhow::Result<()> {
        if !self.model_runtime().check_auth(&model.provider).await {
            anyhow::bail!("No API key for {}/{}", model.provider, model.id);
        }
        let level = self.thinking_level_for_switch(&model, None);
        let previous = self.model();
        self.apply_model(model.clone(), level, persist);
        if let Some(event) = model_select_event(&model, previous.as_ref(), "set")
            && let Some(runner) = self.extension_runner()
        {
            runner.emit(event).await;
        }
        Ok(())
    }

    fn apply_model(&self, model: Model, level: ThinkingLevel, persist: bool) {
        if let Err(error) = self.session_manager().append_model_change(&model.provider, &model.id) {
            eprintln!("Warning: could not save session: {error:#}");
        }
        if persist {
            self.settings().set_default_model_and_provider(&model.provider, &model.id);
            self.add_persisted_default_to_scope(&model);
        }
        self.agent.update_state(|state| state.model = model.clone());
        self.model_selected.store(true, std::sync::atomic::Ordering::Relaxed);
        self.set_thinking_level(level, false);
    }

    fn add_persisted_default_to_scope(&self, model: &Model) {
        let mut scoped = lock(&self.scoped_models);
        if scoped.is_empty() || scoped.iter().any(|entry| models_are_equal(&entry.model, model)) {
            return;
        }
        scoped.push(ScopedModel { model: model.clone(), thinking_level: None });
        let Some(enabled) = self.settings().enabled_models().filter(|enabled| !enabled.is_empty()) else { return };
        let reference = format!("{}/{}", model.provider, model.id);
        if enabled.iter().any(|pattern| pattern.eq_ignore_ascii_case(&reference)) {
            return;
        }
        let mut patterns = enabled;
        patterns.push(reference);
        self.settings().set_enabled_models(Some(&patterns));
    }

    fn thinking_level_for_switch(&self, model: &Model, explicit: Option<ThinkingLevel>) -> ThinkingLevel {
        explicit
            .or_else(|| self.settings().model_thinking_level(&model.provider, &model.id))
            .or_else(|| self.settings().default_thinking_level())
            .unwrap_or_else(|| if self.model().is_some() { self.thinking_level() } else { DEFAULT_THINKING_LEVEL })
    }

    /// Cycle forward (or backward) through the scoped models, or through the available models
    /// when there is no scope. `None` when there is nothing to cycle to.
    pub fn cycle_model(&self, backward: bool, persist: bool) -> Option<ModelCycleResult> {
        let target = self.next_cycle_model(backward)?;
        Some(self.apply_cycle_target(target, persist))
    }

    /// What `cycle_model` would switch to, without switching.
    pub(crate) fn next_cycle_model(&self, backward: bool) -> Option<ModelCycleResult> {
        let (next, is_scoped) = self.cycle_target(backward)?;
        let thinking_level = self.thinking_level_for_switch(&next.model, next.thinking_level);
        Some(ModelCycleResult { model: next.model, thinking_level, is_scoped })
    }

    /// Switches to `target` exactly, as a cycle does, without choosing a target again.
    pub(crate) fn apply_cycle_target(&self, target: ModelCycleResult, persist: bool) -> ModelCycleResult {
        let current = self.model();
        self.apply_model(target.model.clone(), target.thinking_level, persist);
        if let Some(event) = model_select_event(&target.model, current.as_ref(), "cycle") {
            if let Some(runner) = self.extension_runner() {
                runner.emit_detached(event);
            }
        }
        ModelCycleResult { model: target.model, thinking_level: self.thinking_level(), is_scoped: target.is_scoped }
    }

    /// The thinking level that `set_model` gives `model`.
    pub(crate) fn switch_thinking_level(&self, model: &Model) -> ThinkingLevel {
        self.thinking_level_for_switch(model, None)
    }

    fn cycle_target(&self, backward: bool) -> Option<(ScopedModel, bool)> {
        let scoped = self.scoped_models();
        let is_scoped = !scoped.is_empty();
        let available = self.model_runtime().available_models();
        let candidates: Vec<ScopedModel> = if is_scoped {
            scoped
                .into_iter()
                .filter(|entry| available.iter().any(|model| models_are_equal(model, &entry.model)))
                .collect()
        } else {
            available.into_iter().map(|model| ScopedModel { model, thinking_level: None }).collect()
        };
        if candidates.len() <= 1 {
            return None;
        }
        let current = self.model();
        let index = current
            .as_ref()
            .and_then(|current| candidates.iter().position(|entry| models_are_equal(&entry.model, current)))
            .unwrap_or(0);
        let len = candidates.len();
        let next = if backward { (index + len - 1) % len } else { (index + 1) % len };
        Some((candidates[next].clone(), is_scoped))
    }

    /// Thinking levels the current model supports.
    pub fn available_thinking_levels(&self) -> Vec<ThinkingLevel> {
        match self.model() {
            Some(model) => supported_thinking_levels(&model),
            None => ThinkingLevel::ALL.to_vec(),
        }
    }

    pub fn supports_thinking(&self) -> bool {
        self.model().is_some_and(|model| model.reasoning)
    }

    /// `level` clamped to the levels the current model supports.
    pub(crate) fn clamp_thinking_level(&self, level: ThinkingLevel) -> ThinkingLevel {
        if self.available_thinking_levels().contains(&level) {
            return level;
        }
        self.model().map_or(ThinkingLevel::Off, |model| clamp_thinking_level(&model, level))
    }

    /// Set the thinking level, clamped to the model; recorded when it changes. `persist` saves
    /// the requested level as `defaultThinkingLevel`.
    pub fn set_thinking_level(&self, level: ThinkingLevel, persist: bool) {
        let effective = self.clamp_thinking_level(level);
        let before = self.thinking_level();
        self.agent.update_state(|state| state.thinking_level = effective);
        if persist {
            self.settings().set_default_thinking_level(level);
        }
        if effective != before {
            if let Err(error) = self.session_manager().append_thinking_level_change(effective) {
                eprintln!("Warning: could not save session: {error:#}");
            }
            self.emit(&AgentSessionEvent::ThinkingLevelChanged { level: effective });
            if let Some(runner) = self.extension_runner() {
                let event = serde_json::json!({
                    "type": "thinking_level_select",
                    "level": effective.as_str(),
                    "previousLevel": before.as_str(),
                });
                runner.emit_detached(event);
            }
        }
    }

    /// Whether fast mode is on (pi-rust extension). Providers apply it only where
    /// [`Self::fast_mode_available`] holds.
    pub fn fast_mode(&self) -> bool {
        self.with_state(|state| state.fast)
    }

    pub fn fast_mode_available(&self) -> bool {
        self.model().is_some_and(|model| supports_fast(&model))
    }

    /// Turn fast mode on or off; `persist` saves it as `fastMode`. Not recorded in the session.
    pub fn set_fast_mode(&self, enabled: bool, persist: bool) {
        let before = self.fast_mode();
        self.agent.update_state(|state| state.fast = enabled);
        if persist {
            self.settings().set_fast_mode(enabled);
        }
        if enabled != before {
            self.emit(&AgentSessionEvent::FastModeChanged { enabled });
        }
    }

    /// Cycle to the next supported thinking level; `None` when the model has no reasoning.
    pub fn cycle_thinking_level(&self, persist: bool) -> Option<ThinkingLevel> {
        if !self.supports_thinking() {
            return None;
        }
        let levels = self.available_thinking_levels();
        let current = self.thinking_level();
        let index = levels.iter().position(|level| *level == current).map_or(0, |index| index + 1);
        let next = levels[index % levels.len()];
        self.set_thinking_level(next, persist);
        Some(next)
    }

    pub fn steering_mode(&self) -> QueueMode {
        self.settings().steering_mode()
    }

    pub fn follow_up_mode(&self) -> QueueMode {
        self.settings().follow_up_mode()
    }

    /// Set and save how steering messages are released (`all` or `one-at-a-time`).
    pub fn set_steering_mode(&self, mode: QueueMode) {
        self.settings().set_steering_mode(mode);
        self.sync_queue_modes();
    }

    pub fn set_follow_up_mode(&self, mode: QueueMode) {
        self.settings().set_follow_up_mode(mode);
        self.sync_queue_modes();
    }

    pub(crate) fn sync_queue_modes(&self) {
        self.agent.set_queue_modes(self.settings().steering_mode(), self.settings().follow_up_mode());
    }
}
