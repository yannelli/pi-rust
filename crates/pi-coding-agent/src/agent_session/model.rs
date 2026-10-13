//! Model and thinking level changes (upstream `setModel`, `cycleModel`, `setThinkingLevel`,
//! `cycleThinkingLevel`), each recorded in the session.

use pi_agent::QueueMode;
use pi_ai::models::{clamp_thinking_level, models_are_equal, supported_thinking_levels};
use pi_ai::{Model, ThinkingLevel};

use super::{AgentSession, AgentSessionEvent, lock};
use crate::model_resolver::ScopedModel;
use crate::settings::DEFAULT_THINKING_LEVEL;

/// Result of [`AgentSession::cycle_model`].
#[derive(Clone, Debug, PartialEq)]
pub struct ModelCycleResult {
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    /// Whether the cycle went through the scoped models (`--models`) or all available ones.
    pub is_scoped: bool,
}

impl AgentSession {
    /// Switch to `model`. The thinking level becomes the model's `modelThinkingLevels` entry, the
    /// `defaultThinkingLevel` setting, or the current level. `persist` saves the default model.
    pub async fn set_model(&self, model: Model, persist: bool) -> anyhow::Result<()> {
        if !self.model_runtime().check_auth(&model.provider).await {
            anyhow::bail!("No API key for {}/{}", model.provider, model.id);
        }
        let level = self.thinking_level_for_switch(&model, None);
        self.apply_model(model, level, persist);
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
        let next = candidates[next].clone();
        let level = self.thinking_level_for_switch(&next.model, next.thinking_level);
        self.apply_model(next.model.clone(), level, persist);
        Some(ModelCycleResult { model: next.model, thinking_level: self.thinking_level(), is_scoped })
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

    /// Set the thinking level, clamped to the model; recorded when it changes. `persist` saves
    /// the requested level as `defaultThinkingLevel`.
    pub fn set_thinking_level(&self, level: ThinkingLevel, persist: bool) {
        let available = self.available_thinking_levels();
        let effective = if available.contains(&level) {
            level
        } else {
            self.model().map_or(ThinkingLevel::Off, |model| clamp_thinking_level(&model, level))
        };
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
