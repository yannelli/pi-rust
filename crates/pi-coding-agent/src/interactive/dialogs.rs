//! Selectors opened by commands and keys: model, thinking, trust, fork, and clone (upstream
//! `showModelSelector`, `showThinkingSelector`, `showTrustSelector`, `showUserMessageSelector`,
//! and `handleCloneCommand`).

use std::rc::Rc;
use std::sync::Arc;

use pi_ai::{Model, ThinkingLevel};
use pi_tui::component;

use super::mode::InteractiveMode;
use super::selector::SelectorSpec;
use crate::agent_session::{ForkPosition, ModelRuntime};
use crate::interactive_components::model_selector::DefaultModelReference;
use crate::interactive_components::{
    ModelSelector, ModelSelectorOptions, ModelSelectorSource, ThinkingSelector, TrustSelector, TrustSelectorOptions,
    UserMessageItem, UserMessageSelector,
};
use crate::resources::brand::APP_NAME;
use crate::resources::trust::ProjectTrustStore;
use crate::settings::DEFAULT_THINKING_LEVEL;

/// The model runtime as a selector source.
pub(super) struct RuntimeSource(pub Arc<ModelRuntime>);

impl ModelSelectorSource for RuntimeSource {
    fn available_models(&self) -> Vec<Model> {
        self.0.available_models()
    }

    fn find_model(&self, provider: &str, id: &str) -> Option<Model> {
        self.0.get_model(provider, id)
    }

    fn catalog_error(&self) -> Option<String> {
        self.0.load_error().map(str::to_string)
    }
}

impl InteractiveMode {
    /// `/thinking [level]`: set it directly, or open the selector. For an ACP agent, its session opens
    /// first, so the levels are the ones it advertises for the current model.
    pub(super) fn handle_thinking_command(&self, search: Option<String>) {
        if !self.current_model_is_acp() {
            return self.thinking_command(search);
        }
        self.spawn(move |mode| async move {
            if mode.prepare_acp_session().await.is_some() {
                mode.thinking_command(search);
            }
        });
    }

    fn thinking_command(&self, search: Option<String>) {
        let Some(search) = search else { return self.show_thinking_selector() };
        let available = self.session().available_thinking_levels();
        let wanted = search.trim().to_lowercase();
        match available.iter().find(|level| level.as_str() == wanted) {
            Some(level) => self.select_thinking_level(*level, false),
            None => {
                let names: Vec<&str> = available.iter().map(|level| level.as_str()).collect();
                let names = names.join(", ");
                self.show_error(&format!("Unknown thinking level \"{search}\". Available levels: {names}."));
            }
        }
    }

    /// `/fast [on|off]`: set fast mode, or toggle it without an argument; the choice is saved.
    pub(super) fn handle_fast_command(&self, choice: Option<&str>) {
        let session = self.session();
        let enabled = match choice.map(str::to_lowercase).as_deref() {
            None => !session.fast_mode(),
            Some("on") => true,
            Some("off") => false,
            Some(other) => return self.show_error(&format!("Unknown fast mode \"{other}\". Use on or off.")),
        };
        session.set_fast_mode(enabled, true);
        let status = match (enabled, session.fast_mode_available()) {
            (false, _) => "Fast mode off",
            (true, true) => "Fast mode on",
            (true, false) => "Fast mode on (the current model has no fast mode)",
        };
        self.show_status(status);
    }

    /// For an ACP agent, the agent sets the level first, and the level it confirms is the one applied.
    fn select_thinking_level(&self, level: ThinkingLevel, persist: bool) {
        if !self.current_model_is_acp() {
            return self.finish_thinking_level(level, persist);
        }
        self.spawn(move |mode| async move {
            if let Some(confirmed) = mode.set_acp_thinking_level(level).await {
                mode.finish_thinking_level(confirmed, persist);
            }
        });
    }

    fn finish_thinking_level(&self, level: ThinkingLevel, persist: bool) {
        self.session().set_thinking_level(level, persist);
        self.update_editor_border_color();
        let label = if persist { "Default thinking level" } else { "Thinking level" };
        self.show_status(&format!("{label}: {}", level.as_str()));
    }

    fn show_thinking_selector(&self) {
        let session = self.session();
        let current = session.thinking_level();
        let available = session.available_thinking_levels();
        let default = session.settings().default_thinking_level().unwrap_or(DEFAULT_THINKING_LEVEL);
        let weak = self.weak.clone();
        self.selector.show(Box::new(move |done| {
            let (select, persist, cancel) = (weak.clone(), weak.clone(), weak);
            let (done_select, done_persist, done_cancel) = (done.clone(), done.clone(), done);
            let selector = ThinkingSelector::new(
                current,
                &available,
                move |level| {
                    done_select();
                    if let Some(mode) = select.upgrade() {
                        mode.select_thinking_level(level, false);
                    }
                },
                move || {
                    done_cancel();
                    if let Some(mode) = cancel.upgrade() {
                        mode.request_render();
                    }
                },
                Some(Box::new(move |level| {
                    done_persist();
                    if let Some(mode) = persist.upgrade() {
                        mode.select_thinking_level(level, true);
                    }
                })),
                Some(default),
            );
            SelectorSpec::new(component(selector))
        }));
    }

    pub(super) fn apply_model(&self, model: Model, persist: bool) {
        let session = self.session();
        self.spawn(move |mode| async move {
            let id = format!("{}/{}", model.provider, model.id);
            let short = model.id.clone();
            match tokio::spawn(async move { session.set_model(model, persist).await }).await {
                Ok(Ok(())) => {
                    mode.update_available_provider_count();
                    mode.update_editor_border_color();
                    mode.show_status(&if persist { format!("Default model: {id}") } else { format!("Model: {short}") });
                    mode.prepare_acp_session().await;
                }
                Ok(Err(error)) => mode.show_error(&error.to_string()),
                Err(error) => mode.show_error(&error.to_string()),
            }
        });
    }

    pub(super) fn show_model_selector(&self, search: Option<String>) {
        let session = self.session();
        let settings = session.settings().clone();
        let default_model = match (settings.default_provider(), settings.default_model()) {
            (Some(provider), Some(id)) => Some(DefaultModelReference { provider, id }),
            _ => None,
        };
        let source: Rc<dyn ModelSelectorSource> = Rc::new(RuntimeSource(session.model_runtime().clone()));
        let refresh_runtime = session.model_runtime().clone();
        let tui = self.tui();
        let weak = self.weak.clone();
        self.selector.show(Box::new(move |done| {
            let (select, persist, cancel) = (weak.clone(), weak.clone(), weak);
            let (done_select, done_persist, done_cancel) = (done.clone(), done.clone(), done);
            let selector = ModelSelector::new(
                &tui,
                session.model(),
                source,
                session.scoped_models(),
                move |model| {
                    done_select();
                    if let Some(mode) = select.upgrade() {
                        mode.apply_model(model.clone(), false);
                    }
                },
                move || {
                    done_cancel();
                    if let Some(mode) = cancel.upgrade() {
                        mode.request_render();
                    }
                },
                ModelSelectorOptions {
                    initial_search_input: search,
                    on_select_as_default: Some(Box::new(move |model: &Model| {
                        done_persist();
                        if let Some(mode) = persist.upgrade() {
                            mode.apply_model(model.clone(), true);
                        }
                    })),
                    default_model,
                    refresh: Some(super::model_refresh::selector_refresh(refresh_runtime)),
                },
            );
            let dispose = selector.borrow().disposer();
            SelectorSpec::new(selector).with_dispose(Box::new(dispose))
        }));
    }

    /// `/trust`: save a decision for this folder or a parent.
    pub(super) fn show_trust_selector(&self) {
        let session = self.session();
        let cwd = session.cwd().to_path_buf();
        let store = ProjectTrustStore::new(&self.agent_dir);
        let saved = store.get_entry(&cwd).ok().flatten();
        let trusted = session.settings().is_project_trusted();
        let weak = self.weak.clone();
        self.selector.show(Box::new(move |done| {
            let (select, cancel) = (weak.clone(), weak);
            let (done_select, done_cancel) = (done.clone(), done);
            let selector = TrustSelector::new(TrustSelectorOptions {
                cwd,
                saved_decision: saved,
                project_trusted: trusted,
                on_select: Box::new(move |selection| {
                    done_select();
                    let Some(mode) = select.upgrade() else { return };
                    if let Err(error) = store.set_many(&selection.updates) {
                        return mode.show_error(&format!("Could not save trust decision: {error:#}"));
                    }
                    let label = if selection.trusted { "trusted" } else { "untrusted" };
                    let restart = format!("Restart {APP_NAME} for this to take effect.");
                    mode.show_status(&format!("Saved trust decision: {label}. {restart}"));
                }),
                on_cancel: Box::new(move || {
                    done_cancel();
                    if let Some(mode) = cancel.upgrade() {
                        mode.request_render();
                    }
                }),
            });
            SelectorSpec::new(component(selector))
        }));
    }

    /// `/fork` and its key: copy the path up to a user message into a new session.
    pub(super) fn show_user_message_selector(&self) {
        let messages = self.session().user_messages_for_forking();
        if messages.is_empty() {
            return self.show_status("No messages to fork from");
        }
        let initial = messages.last().map(|(id, _)| id.clone());
        let items: Vec<UserMessageItem> = messages.iter().map(|(id, text)| UserMessageItem::new(id, text)).collect();
        let weak = self.weak.clone();
        self.selector.show(Box::new(move |done| {
            let (select, cancel) = (weak.clone(), weak);
            let (done_select, done_cancel) = (done.clone(), done);
            let selector = UserMessageSelector::new(
                items,
                move |entry_id| {
                    done_select();
                    let Some(mode) = select.upgrade() else { return };
                    let runtime = mode.runtime.clone();
                    mode.spawn(move |mode| async move {
                        match tokio::spawn(async move { runtime.fork(&entry_id, ForkPosition::Before).await }).await {
                            Ok(Ok(result)) if result.cancelled => mode.request_render(),
                            Ok(Ok(result)) => {
                                mode.editor.set_text(result.selected_text.as_deref().unwrap_or(""));
                                mode.show_status("Forked to new session");
                            }
                            Ok(Err(error)) => mode.show_error(&error.to_string()),
                            Err(error) => mode.show_error(&error.to_string()),
                        }
                    });
                },
                move || {
                    done_cancel();
                    if let Some(mode) = cancel.upgrade() {
                        mode.request_render();
                    }
                },
                initial.as_deref(),
            );
            let list = selector.message_list();
            SelectorSpec::new(component(selector)).with_focus(list)
        }));
    }

    /// `/clone`: a new session with the current branch.
    pub(super) fn handle_clone_command(&self) {
        if self.session().session_manager().leaf_id().is_none() {
            return self.show_status("Nothing to clone yet");
        }
        let runtime = self.runtime.clone();
        self.spawn(move |mode| async move {
            match tokio::spawn(async move { runtime.clone_session().await }).await {
                Ok(Ok(result)) if result.cancelled => mode.request_render(),
                Ok(Ok(_)) => {
                    mode.editor.set_text("");
                    mode.show_status("Cloned to new session");
                }
                Ok(Err(error)) => mode.show_error(&error.to_string()),
                Err(error) => mode.show_error(&error.to_string()),
            }
        });
    }
}
