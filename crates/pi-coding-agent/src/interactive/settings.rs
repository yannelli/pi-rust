//! `/settings` and `/scoped-models` (upstream `showSettingsSelector` and `showModelsSelector`).

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::{Rc, Weak};

use pi_ai::Model;
use pi_tui::{Component, component};
use tokio_util::sync::CancellationToken;

use super::components::{for_each_tool, set_component_output_pad};
use super::layout::{renderer_mode, scrollbar_setting, wheel_lines};
use super::mode::InteractiveMode;
use super::model_refresh::{RefreshOutcome, refresh_catalogs};
use super::selector::SelectorSpec;
use crate::agent_session::auth::configure_http_from_settings;
use crate::interactive_components::scoped_models_selector::{
    EnabledIds, ModelsCallbacks, ModelsConfig, RefreshStatusKind,
};
use crate::interactive_components::settings_selector::{
    DefaultProjectTrust, SettingsCallbacks, SettingsConfig, format_http_idle_timeout_ms,
};
use crate::interactive_components::{ScopedModelsSelector, SettingsSelector};
use crate::model_resolver::{ModelScopeCode, ScopedModel, resolve_model_scope};
use crate::settings::DEFAULT_THINKING_LEVEL;
use crate::settings::defaults::DEFAULT_HTTP_IDLE_TIMEOUT_MS;
use crate::settings::schema::TuiMode;
use crate::theme::{SYSTEM_THEME_NAME, get_available_themes};

/// The settings value of a choice, such as `fullscreen` or `idle`.
fn setting_str(value: impl serde::Serialize) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(text)) => text,
        _ => String::new(),
    }
}

/// Sets the value a setting shows, such as reverting a rejected change.
type ValueUpdater = Rc<dyn Fn(&str, &str)>;

/// What `/scoped-models` shares between its callbacks and the catalog refresh.
struct ScopedState {
    models: Vec<Model>,
    enabled: EnabledIds,
    changed: bool,
}

fn model_key(model: &Model) -> String {
    format!("{}/{}", model.provider, model.id)
}

/// Runs `apply` with the mode when it is still alive.
fn with(weak: &Weak<InteractiveMode>, apply: impl FnOnce(&Rc<InteractiveMode>)) {
    if let Some(mode) = weak.upgrade() {
        apply(&mode);
    }
}

/// A settings callback that upgrades the mode first.
macro_rules! on {
    ($weak:expr, |$mode:ident, $value:ident| $body:expr) => {{
        let weak = $weak.clone();
        Box::new(move |$value| with(&weak, |$mode| $body))
    }};
}

impl InteractiveMode {
    fn settings_config(&self) -> SettingsConfig {
        let session = self.session();
        let settings = self.settings();
        let default_model = match (settings.default_provider(), settings.default_model()) {
            (Some(provider), Some(id)) => format!("{provider}/{id}"),
            _ => "not set".to_string(),
        };
        let controller = self.theme_controller();
        SettingsConfig {
            auto_compact: session.auto_compaction_enabled(),
            default_model,
            current_model: session.model(),
            available_default_models: session.model_runtime().available_models(),
            show_images: settings.show_images(),
            image_width_cells: settings.image_width_cells(),
            auto_resize_images: settings.image_auto_resize(),
            block_images: settings.block_images(),
            enable_skill_commands: settings.enable_skill_commands(),
            steering_mode: session.steering_mode(),
            follow_up_mode: session.follow_up_mode(),
            transport: settings.transport(),
            session_share_destination: settings.session_share_destination(),
            http_idle_timeout_ms: settings.http_idle_timeout_ms().unwrap_or(DEFAULT_HTTP_IDLE_TIMEOUT_MS),
            cache_warming_mode: settings.cache_warming_mode(),
            thinking_level: settings.default_thinking_level().unwrap_or(DEFAULT_THINKING_LEVEL),
            available_thinking_levels: pi_ai::ThinkingLevel::ALL.to_vec(),
            model_thinking_levels: settings.all_model_thinking_levels(),
            current_theme: controller
                .as_ref()
                .and_then(|controller| controller.get_theme_selection())
                .unwrap_or_else(|| SYSTEM_THEME_NAME.to_string()),
            terminal_theme: controller
                .map(|controller| controller.get_terminal_theme())
                .unwrap_or(crate::theme::TerminalTheme::Dark),
            available_themes: get_available_themes(),
            hide_thinking_block: self.state.borrow().hide_thinking_block,
            mermaid_rendering_mode: settings.mermaid_rendering_mode(),
            show_cache_miss_notices: settings.show_cache_miss_notices(),
            collapse_changelog: settings.collapse_changelog(),
            enable_install_telemetry: settings.enable_install_telemetry(),
            double_escape_action: settings.double_escape_action(),
            tree_filter_mode: settings.tree_filter_mode(),
            show_hardware_cursor: settings.show_hardware_cursor(),
            editor_padding_x: settings.editor_padding_x(),
            output_pad: settings.output_pad(),
            autocomplete_max_visible: settings.autocomplete_max_visible(),
            quiet_startup: settings.quiet_startup(),
            default_project_trust: DefaultProjectTrust::from_setting(settings.default_project_trust()),
            clear_on_shrink: settings.clear_on_shrink(),
            show_terminal_progress: settings.show_terminal_progress(),
            tui_mode: self.tui_mode(),
            fullscreen_exit_output: settings.fullscreen_exit_output(),
            fullscreen_scrollbar: settings.fullscreen_scrollbar(),
            fullscreen_copy_on_select: settings.fullscreen_copy_on_select(),
            fullscreen_wheel_scroll_lines: settings.fullscreen_wheel_scroll_lines(),
            warnings: settings.settings().warnings.unwrap_or_default(),
        }
    }

    /// The model override for `provider/id` changed; apply it when that is the current model.
    fn apply_model_thinking(&self, provider: &str, id: &str, level: Option<pi_ai::ThinkingLevel>) {
        let session = self.session();
        let Some(current) = session.model() else { return };
        if current.provider != provider || current.id != id {
            return;
        }
        let level = level.or_else(|| self.settings().default_thinking_level()).unwrap_or(DEFAULT_THINKING_LEVEL);
        session.set_thinking_level(level, false);
        self.footer.borrow_mut().invalidate();
        self.update_editor_border_color();
    }

    fn change_tui_mode(&self, mode: TuiMode, revert: &dyn Fn(&str, &str)) {
        let tui = self.tui();
        if renderer_mode(mode) != tui.mode() && tui.has_overlay() {
            revert("tui-mode", &setting_str(self.tui_mode()));
            return self.show_status("Close active overlays before changing TUI mode");
        }
        self.settings().set_tui_mode(mode);
        self.defer(move |this| {
            this.switch_tui_mode(mode, true, true);
            if this.state.borrow().active_status.is_none() {
                this.ui.status.borrow_mut().clear();
            }
            this.show_status(&format!("TUI mode: {}", setting_str(mode)));
        });
    }

    fn settings_callbacks(&self, revert: ValueUpdater, done: Rc<dyn Fn()>) -> SettingsCallbacks {
        let weak = self.weak.clone();
        SettingsCallbacks {
            on_auto_compact_change: on!(weak, |mode, enabled| {
                mode.session().set_auto_compaction_enabled(enabled);
                mode.footer.borrow_mut().set_auto_compact_enabled(enabled);
            }),
            on_show_images_change: on!(weak, |mode, enabled| {
                mode.settings().set_show_images(enabled);
                for_each_tool(&mode.ui.chat.borrow().children, |tool| tool.set_show_images(enabled));
            }),
            on_image_width_cells_change: on!(weak, |mode, width| {
                mode.settings().set_image_width_cells(width);
                for_each_tool(&mode.ui.chat.borrow().children, |tool| tool.set_image_width_cells(width as usize));
            }),
            on_auto_resize_images_change: on!(weak, |mode, enabled| mode.settings().set_image_auto_resize(enabled)),
            on_block_images_change: on!(weak, |mode, blocked| mode.settings().set_block_images(blocked)),
            on_enable_skill_commands_change: on!(weak, |mode, enabled| {
                mode.settings().set_enable_skill_commands(enabled);
                mode.setup_autocomplete_provider();
            }),
            on_steering_mode_change: on!(weak, |mode, queue| mode.session().set_steering_mode(queue)),
            on_follow_up_mode_change: on!(weak, |mode, queue| mode.session().set_follow_up_mode(queue)),
            on_transport_change: on!(weak, |mode, transport| mode.settings().set_transport(transport)),
            on_session_share_destination_change: on!(weak, |mode, destination| {
                mode.settings().set_session_share_destination(destination)
            }),
            on_http_idle_timeout_ms_change: on!(weak, |mode, timeout| {
                let settings = mode.settings();
                settings.set_http_idle_timeout_ms(timeout);
                configure_http_from_settings(&settings);
                mode.show_status(&format!("HTTP idle timeout: {}", format_http_idle_timeout_ms(timeout)));
            }),
            on_cache_warming_mode_change: on!(weak, |mode, warming| {
                mode.session().set_cache_warming_mode(warming);
                mode.show_status(&format!("Cache warming: {}", setting_str(warming)));
            }),
            on_model_thinking_level_change: {
                let weak = weak.clone();
                Box::new(move |provider, id, level| {
                    with(&weak, |mode| {
                        mode.settings().set_model_thinking_level(provider, id, level);
                        mode.apply_model_thinking(provider, id, Some(level));
                    })
                })
            },
            on_model_thinking_level_remove: {
                let weak = weak.clone();
                Box::new(move |provider, id| {
                    with(&weak, |mode| {
                        mode.settings().remove_model_thinking_level(provider, id);
                        mode.apply_model_thinking(provider, id, None);
                    })
                })
            },
            on_theme_change: {
                let weak = weak.clone();
                Box::new(move |setting: &str| {
                    let setting = setting.to_string();
                    with(&weak, |mode| {
                        mode.settings().set_theme(&setting);
                        mode.defer(move |mode| {
                            if let Some(controller) = mode.theme_controller() {
                                controller.set_theme_setting(&setting);
                            }
                        });
                    })
                })
            },
            on_theme_preview: Some({
                let weak = weak.clone();
                Box::new(move |setting: &str| {
                    let setting = setting.to_string();
                    with(&weak, |mode| {
                        mode.defer(move |mode| {
                            if let Some(controller) = mode.theme_controller() {
                                controller.preview(&setting);
                            }
                        })
                    })
                })
            }),
            on_hide_thinking_block_change: on!(weak, |mode, hidden| {
                mode.state.borrow_mut().hide_thinking_block = hidden;
                mode.settings().set_hide_thinking_block(hidden);
                mode.update_thinking_block_visibility();
            }),
            on_mermaid_rendering_mode_change: on!(weak, |mode, rendering| {
                mode.settings().set_mermaid_rendering_mode(rendering);
                mode.ui.chat.borrow_mut().invalidate();
                mode.request_render();
            }),
            on_show_cache_miss_notices_change: on!(weak, |mode, shown| {
                mode.settings().set_show_cache_miss_notices(shown);
                mode.rebuild_chat_from_messages();
            }),
            on_collapse_changelog_change: on!(weak, |mode, collapsed| mode
                .settings()
                .set_collapse_changelog(collapsed)),
            on_enable_install_telemetry_change: on!(weak, |mode, enabled| {
                mode.settings().set_enable_install_telemetry(enabled)
            }),
            on_double_escape_action_change: on!(weak, |mode, action| mode.settings().set_double_escape_action(action)),
            on_tree_filter_mode_change: on!(weak, |mode, filter| mode.settings().set_tree_filter_mode(filter)),
            on_show_hardware_cursor_change: on!(weak, |mode, enabled| {
                mode.settings().set_show_hardware_cursor(enabled);
                mode.tui().set_show_hardware_cursor(enabled);
            }),
            on_editor_padding_x_change: on!(weak, |mode, padding| {
                mode.settings().set_editor_padding_x(padding);
                mode.editor.editor().set_padding_x(padding as usize);
            }),
            on_output_pad_change: on!(weak, |mode, padding| {
                mode.settings().set_output_pad(padding);
                mode.state.borrow_mut().output_pad = usize::from(padding);
                for container in [&mode.ui.chat, &mode.ui.pending] {
                    for child in &container.borrow().children {
                        set_component_output_pad(child, usize::from(padding));
                    }
                }
                mode.request_render();
            }),
            on_autocomplete_max_visible_change: on!(weak, |mode, max_visible| {
                mode.settings().set_autocomplete_max_visible(max_visible);
                mode.editor.editor().set_autocomplete_max_visible(max_visible as usize);
            }),
            on_quiet_startup_change: on!(weak, |mode, quiet| mode.settings().set_quiet_startup(quiet)),
            on_default_project_trust_change: on!(weak, |mode, trust| {
                mode.settings().set_default_project_trust(trust.as_str())
            }),
            on_clear_on_shrink_change: on!(weak, |mode, enabled| {
                mode.settings().set_clear_on_shrink(enabled);
                mode.tui().set_clear_on_shrink(enabled);
                if !enabled && mode.state.borrow().active_status.is_none() {
                    mode.ui.status.borrow_mut().clear();
                }
            }),
            on_show_terminal_progress_change: on!(weak, |mode, enabled| {
                mode.settings().set_show_terminal_progress(enabled)
            }),
            on_tui_mode_change: {
                let weak = weak.clone();
                Box::new(move |tui_mode| with(&weak, |mode| mode.change_tui_mode(tui_mode, &*revert)))
            },
            on_fullscreen_exit_output_change: on!(weak, |mode, output| mode
                .settings()
                .set_fullscreen_exit_output(output)),
            on_fullscreen_scrollbar_change: on!(weak, |mode, scrollbar| {
                mode.settings().set_fullscreen_scrollbar(scrollbar);
                mode.ui.transcript.set_scrollbar(scrollbar_setting(scrollbar));
                mode.request_render();
            }),
            on_fullscreen_copy_on_select_change: on!(weak, |mode, enabled| {
                mode.settings().set_fullscreen_copy_on_select(enabled);
                let tui = mode.tui();
                if tui.is_viewport() {
                    tui.set_copy_on_select(enabled);
                }
            }),
            on_fullscreen_wheel_scroll_lines_change: on!(weak, |mode, lines| {
                let settings = mode.settings();
                settings.set_fullscreen_wheel_scroll_lines(lines);
                let tui = mode.tui();
                if tui.is_viewport() {
                    tui.set_wheel_scroll_lines(wheel_lines(&settings));
                }
            }),
            on_warnings_change: {
                let weak = weak.clone();
                Box::new(move |warnings| with(&weak, |mode| mode.settings().set_warnings(warnings)))
            },
            on_cancel: Box::new(move || {
                done();
                with(&weak, |mode| mode.request_render());
            }),
        }
    }

    /// `/settings`.
    pub(super) fn show_settings_selector(&self) {
        let weak = self.weak.clone();
        let config = self.settings_config();
        self.selector.show(Box::new(move |done| {
            let revert: Rc<RefCell<Option<ValueUpdater>>> = Rc::default();
            let forward = revert.clone();
            let revert_value: ValueUpdater = Rc::new(move |id: &str, value: &str| {
                if let Some(update) = forward.borrow().as_ref() {
                    update(id, value);
                }
            });
            let Some(mode) = weak.upgrade() else {
                return SelectorSpec::new(component(SettingsSelector::new(config, SettingsCallbacks::default())));
            };
            let callbacks = mode.settings_callbacks(revert_value, done);
            let selector = SettingsSelector::new(config, callbacks);
            *revert.borrow_mut() = Some(Rc::new(selector.value_updater()));
            let list = selector.settings_list();
            SelectorSpec::new(component(selector)).with_focus(list)
        }));
    }

    /// The enabled ids from `enabledModels`, keeping patterns that match nothing.
    fn configured_enabled_ids(&self, models: &[Model]) -> EnabledIds {
        let patterns = self.settings().enabled_models().filter(|patterns| !patterns.is_empty())?;
        let (scoped, diagnostics) = resolve_model_scope(&patterns, models);
        let mut ids: Vec<String> = scoped.iter().map(|scoped| model_key(&scoped.model)).collect();
        for diagnostic in diagnostics {
            if diagnostic.code == ModelScopeCode::NoMatch && !ids.contains(&diagnostic.pattern) {
                ids.push(diagnostic.pattern);
            }
        }
        Some(ids)
    }

    fn update_session_models(&self, state: &Rc<RefCell<ScopedState>>, enabled: EnabledIds) {
        state.borrow_mut().enabled = enabled.clone();
        let models = state.borrow().models.clone();
        let models = models.as_slice();
        let enabled = &enabled;
        let available: HashSet<String> = models.iter().map(model_key).collect();
        let session = self.session();
        let scoped: Vec<ScopedModel> = match enabled {
            Some(ids)
                if ids.iter().any(|id| available.contains(id)) && !available.iter().all(|id| ids.contains(id)) =>
            {
                resolve_model_scope(ids, models).0
            }
            _ => Vec::new(),
        };
        session.set_scoped_models(scoped);
        self.update_available_provider_count();
        self.request_render();
    }

    /// `/scoped-models`: the models Ctrl+P cycles through. The catalogs refresh while it is open.
    pub(super) fn show_models_selector(&self) {
        let session = self.session();
        let models = session.model_runtime().available_models();
        let scoped = session.scoped_models();
        let had_scoped = !scoped.is_empty();
        let enabled = if had_scoped {
            Some(scoped.iter().map(|scoped| model_key(&scoped.model)).collect())
        } else {
            self.configured_enabled_ids(&models)
        };
        let state = Rc::new(RefCell::new(ScopedState { models, enabled, changed: false }));
        let weak = self.weak.clone();
        self.selector.show(Box::new(move |done| {
            let (change, persist, cancel, refresh) = (weak.clone(), weak.clone(), weak.clone(), weak.clone());
            let (change_state, persist_state) = (state.clone(), state.clone());
            let (models, enabled) = {
                let state = state.borrow();
                (state.models.clone(), state.enabled.clone())
            };
            let config = ModelsConfig {
                all_models: models,
                enabled_model_ids: enabled,
                refresh_status: Some("Refreshing model catalogs…".into()),
            };
            let selector = component(ScopedModelsSelector::new(
                config,
                ModelsCallbacks {
                    on_change: Box::new(move |enabled| {
                        change_state.borrow_mut().changed = true;
                        with(&change, |mode| mode.update_session_models(&change_state, enabled))
                    }),
                    on_persist: Box::new(move |enabled| {
                        with(&persist, |mode| {
                            let models = persist_state.borrow().models.clone();
                            let available: HashSet<String> = models.iter().map(model_key).collect();
                            let all = enabled.as_ref().is_some_and(|ids| {
                                ids.len() == models.len() && ids.iter().all(|id| available.contains(id))
                            });
                            let patterns = enabled.filter(|_| !all);
                            mode.settings().set_enabled_models(patterns.as_deref());
                            mode.show_status("Model selection saved to settings");
                        })
                    }),
                    on_cancel: Box::new(move || {
                        done();
                        with(&cancel, |mode| mode.request_render());
                    }),
                },
            ));
            let token = CancellationToken::new();
            let target = Rc::downgrade(&selector);
            let (task_state, task_token) = (state.clone(), token.clone());
            if let Some(mode) = refresh.upgrade() {
                mode.spawn(move |mode| async move {
                    let outcome = refresh_catalogs(mode.session().model_runtime().clone(), task_token.clone()).await;
                    let Some(selector) = target.upgrade().filter(|_| !task_token.is_cancelled()) else { return };
                    mode.apply_scoped_refresh(&task_state, &selector, &outcome, had_scoped);
                });
            }
            SelectorSpec::new(selector).with_dispose(move || token.cancel())
        }));
    }

    fn apply_scoped_refresh(
        &self,
        state: &Rc<RefCell<ScopedState>>,
        selector: &Rc<RefCell<ScopedModelsSelector>>,
        outcome: &RefreshOutcome,
        had_scoped: bool,
    ) {
        let models = self.session().model_runtime().available_models();
        let changed = state.borrow().changed;
        if !changed && !had_scoped {
            let enabled = self.configured_enabled_ids(&models);
            state.borrow_mut().enabled = enabled.clone();
            selector.borrow_mut().update_models(models.clone(), Some(enabled));
        } else {
            selector.borrow_mut().update_models(models.clone(), None);
        }
        state.borrow_mut().models = models;
        let enabled = state.borrow().enabled.clone();
        if enabled.is_some() {
            self.update_session_models(state, enabled);
        }
        match outcome.warning("showing cached models") {
            Some(warning) => selector.borrow_mut().set_refresh_status(&warning, RefreshStatusKind::Warning),
            None => selector.borrow_mut().set_refresh_status("Model catalogs refreshed.", RefreshStatusKind::Success),
        }
        self.request_render();
    }
}
