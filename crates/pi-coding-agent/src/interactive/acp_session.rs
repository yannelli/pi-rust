//! Opens the ACP agent session of the current model before the first prompt, so `/model`, `/thinking`, and
//! `/acp mode` see the models, settings, and thinking levels the agent advertises. The first prompt reuses it.

use std::time::Duration;

use pi_ai::providers::acp::{self, DEFAULT_MODEL, SessionInfo};
use pi_ai::simple_options::{MappedLevel, mapped_thinking_level};
use pi_ai::{Model, ThinkingLevel};

use super::mode::InteractiveMode;

/// How long opening an agent session or setting its model may take.
const OPEN_TIMEOUT: Duration = Duration::from_secs(90);

/// An ACP model whose agent is registered from the settings; other providers are never started.
pub(super) fn is_registered_acp(model: &Model) -> bool {
    model.api == acp::API && acp::agent(&model.provider).is_some()
}

/// The registered ACP provider that a typed `provider/model` reference names.
pub(super) fn referenced_acp_provider(search: &str) -> Option<String> {
    let (provider, _) = search.trim().split_once('/')?;
    acp::agent(provider).map(|agent| agent.provider)
}

impl InteractiveMode {
    /// Whether the current model is a registered ACP agent.
    pub(super) fn current_model_is_acp(&self) -> bool {
        self.session().model().is_some_and(|model| is_registered_acp(&model))
    }

    /// Opens (or reuses) the agent session of `provider` for this pi session. Shows the error and
    /// returns `None` when the agent fails or does not answer in time.
    pub(super) async fn open_acp_session(&self, provider: &str) -> Option<SessionInfo> {
        let pi_session = self.session().session_id();
        match tokio::time::timeout(OPEN_TIMEOUT, acp::open_session(provider, &pi_session)).await {
            Ok(Ok(info)) => Some(info),
            Ok(Err(error)) => {
                self.show_error(&error);
                None
            }
            Err(_) => {
                self.show_error(&format!("{provider} did not start within {} seconds", OPEN_TIMEOUT.as_secs()));
                None
            }
        }
    }

    /// Opens the current ACP model's session, selects that model in the agent unless it is `default`,
    /// and refreshes its thinking levels; `None` for other models or after an error, shown once.
    pub(super) async fn prepare_acp_session(&self) -> Option<SessionInfo> {
        let session = self.session();
        let model = session.model().filter(is_registered_acp)?;
        let mut info = self.open_acp_session(&model.provider).await?;
        if model.id != DEFAULT_MODEL {
            let selector = info.selectors.iter().find(|selector| selector.category == "model");
            let current = selector.and_then(|selector| selector.current.as_deref());
            if current != Some(model.id.as_str()) {
                let id = selector.map_or("model", |selector| selector.id.as_str()).to_string();
                let pi_session = session.session_id();
                let set = acp::set_session_option(&model.provider, &pi_session, &id, &model.id);
                match tokio::time::timeout(OPEN_TIMEOUT, set).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        self.show_error(&error);
                        return None;
                    }
                    Err(_) => {
                        self.show_error(&format!("{} did not select {} in time", model.provider, model.id));
                        return None;
                    }
                }
                info = self.open_acp_session(&model.provider).await?;
            }
        }
        self.apply_acp_session_info(&info);
        Some(info)
    }

    /// Takes the current model's metadata and thinking level from `info`, and redraws what depends on them.
    pub(super) fn apply_acp_session_info(&self, info: &SessionInfo) {
        if self.session().refresh_acp_model_metadata(info) {
            self.update_editor_border_color();
        }
        self.setup_autocomplete_provider();
        self.request_render();
    }

    /// Sets `level` in the agent through the exact choice the model's thinking level map names, and
    /// returns the level the agent then reports. `None` after an error, shown once; nothing is set then.
    pub(super) async fn set_acp_thinking_level(&self, level: ThinkingLevel) -> Option<ThinkingLevel> {
        let info = self.prepare_acp_session().await?;
        let model = self.session().model().filter(is_registered_acp)?;
        let Some(selector) = info.selectors.iter().find(|selector| selector.category == "thought_level") else {
            self.show_error(&format!("{} offers no thinking levels", model.provider));
            return None;
        };
        let value = match mapped_thinking_level(&model, level) {
            MappedLevel::Value(value) if selector.choices.iter().any(|choice| choice.id == value) => value.to_string(),
            _ => {
                self.show_error(&format!("{} has no thinking level for {}", model.provider, level.as_str()));
                return None;
            }
        };
        let pi_session = self.session().session_id();
        let set = acp::set_session_option(&model.provider, &pi_session, &selector.id, &value);
        match tokio::time::timeout(OPEN_TIMEOUT, set).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                self.show_error(&error);
                return None;
            }
            Err(_) => {
                self.show_error(&format!("{} did not set the thinking level in time", model.provider));
                return None;
            }
        }
        let info = self.open_acp_session(&model.provider).await?;
        self.apply_acp_session_info(&info);
        let model = self.session().model()?;
        let thought = info.selectors.iter().find(|selector| selector.category == "thought_level");
        let current = thought.and_then(|selector| selector.current.clone()).unwrap_or_default();
        let maps_to = |candidate: ThinkingLevel| matches!(mapped_thinking_level(&model, candidate), MappedLevel::Value(value) if value == current);
        let confirmed = std::iter::once(level).chain(ThinkingLevel::ALL.iter().copied()).find(|&l| maps_to(l));
        if confirmed.is_none() {
            self.show_error(&format!(
                "{} reports thinking level \"{current}\", which no pi level maps to",
                model.provider
            ));
        }
        confirmed
    }

    /// Opens the current ACP model's session without a prompt: after the extensions are bound, and after a model switch.
    pub(super) fn open_current_acp_session(&self) {
        if self.current_model_is_acp() {
            self.spawn(|mode| async move {
                mode.prepare_acp_session().await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_acp_models_of_registered_agents_qualify() {
        let other = Model { api: "openai-completions".into(), provider: "openai".into(), ..Model::default() };
        assert!(!is_registered_acp(&other));
        let unknown = Model { api: acp::API.into(), provider: "acp-not-registered".into(), ..Model::default() };
        assert!(!is_registered_acp(&unknown));
        assert_eq!(referenced_acp_provider("acp-not-registered/default"), None);
        assert_eq!(referenced_acp_provider("no-slash"), None);
    }
}
