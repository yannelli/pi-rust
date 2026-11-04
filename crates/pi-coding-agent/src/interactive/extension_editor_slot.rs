//! The editor slot and status line under extension control (upstream `setCustomEditorComponent`,
//! `setWorkingMessage`, `setWorkingIndicator`, `setHiddenThinkingLabel`, and `onTerminalInput`),
//! and the mirror of UI state that extensions read from other threads.

use std::cell::RefCell;
use std::rc::Rc;

use pi_tui::components::LoaderIndicatorOptions;
use pi_tui::components::editor::{EditorLayout, PromptBox};
use pi_tui::{ComponentRef, TuiInputListenerResult};
use serde_json::{Value, json};

use super::components::{AssistantMessageComponent, StatusKind, UserMessageComponent};
use super::extension_view::{call_js, dispose};
use super::mode::{DEFAULT_HIDDEN_THINKING_LABEL, DEFAULT_WORKING_MESSAGE, InteractiveMode};
use crate::extension_ui::{ChatStyle, PromptLayout, WorkingIndicatorOptions};
use crate::extensions::{ComponentFactory, TerminalInputVerdict};
use crate::theme::{ThemeColor, theme};

const IDLE_PLACEHOLDER: &str = "Ask anything \u{b7} / for commands \u{b7} @ for files";
const RUNNING_PLACEHOLDER: &str = "Queue a message \u{b7} esc to stop the agent";

impl InteractiveMode {
    pub(super) fn has_custom_editor(&self) -> bool {
        self.state.borrow().extensions.editor.is_some()
    }

    /// The text of the editor shown now.
    pub(super) fn current_editor_text(&self) -> String {
        let custom = self.state.borrow().extensions.editor.clone();
        match custom.and_then(|editor| call_js(&editor, "getText", Vec::new())) {
            Some(Value::String(text)) => text,
            _ => self.editor.get_text(),
        }
    }

    pub(super) fn set_extension_editor_text(&self, text: &str) {
        let custom = self.state.borrow().extensions.editor.clone();
        match custom {
            Some(editor) => drop(call_js(&editor, "setText", vec![json!(text)])),
            None => self.editor.set_text(text),
        }
        self.state.borrow().extensions.mirror.set_editor_text(text);
        self.request_render();
    }

    /// Paste `text` into the editor shown now, as a bracketed paste.
    pub(super) fn paste_to_extension_editor(&self, text: &str) {
        let paste = format!("\x1b[200~{text}\x1b[201~");
        let custom = self.state.borrow().extensions.editor.clone();
        match custom {
            Some(editor) => drop(call_js(&editor, "handleInput", vec![json!(paste)])),
            None => self.editor.handle_input(&paste),
        }
        self.request_render();
    }

    /// While an extension dialog is open, the new editor waits behind it and shows when the last
    /// dialog closes, so the dialog stays visible and answerable (upstream #11431).
    pub(super) fn set_custom_editor(&self, factory: Option<ComponentFactory>) {
        let text = self.current_editor_text();
        if !self.dialog_open() {
            self.selector.dispose_active();
        }
        {
            let mut state = self.extensions_state();
            if state.default_editor.is_none() && state.editor.is_none() {
                state.default_editor = Some(self.selector.editor());
            }
        }
        let Some(factory) = factory else {
            let restored = {
                let mut state = self.extensions_state();
                state.bump("editor");
                if let Some(old) = state.editor.take() {
                    dispose(&old);
                }
                state.default_editor.clone()
            };
            self.editor.set_text(&text);
            if let Some(default) = restored {
                self.mount_editor(default);
            }
            return;
        };
        let settings = self.settings();
        let (padding, visible) = (settings.editor_padding_x(), settings.autocomplete_max_visible());
        self.build_component("editor".into(), factory, move |mode, js| {
            let editor = Rc::new(RefCell::new(js));
            call_js(&editor, "setText", vec![json!(text)]);
            call_js(&editor, "setPaddingX", vec![json!(padding)]);
            call_js(&editor, "setAutocompleteMaxVisible", vec![json!(visible)]);
            if let Some(old) = mode.extensions_state().editor.replace(editor.clone()) {
                dispose(&old);
            }
            mode.mount_editor(editor);
        });
    }

    fn mount_editor(&self, editor: ComponentRef) {
        let dialog_open = self.dialog_open();
        if !dialog_open {
            self.selector.dispose_active();
        }
        self.selector.set_editor(editor.clone());
        self.place_status_indicator();
        if !dialog_open {
            self.tui().set_focus(Some(editor));
        }
        self.request_render();
    }

    /// Whether the working status goes into the editor border: only the default editor draws it,
    /// and a harbor box keeps its border free of labels.
    pub(super) fn embeds_working_status(&self) -> bool {
        let harbor = self.state.borrow().chat_style.prompt_layout == Some(PromptLayout::Harbor);
        self.editor.embed_working_status() && !self.has_custom_editor() && !harbor
    }

    /// Put the active status indicator where the editor shown now wants it.
    fn place_status_indicator(&self) {
        let Some(active) = self.state.borrow().active_status.clone() else { return };
        self.ui.status.borrow_mut().clear();
        self.editor.set_working_status_indicator(None);
        let embed = self.embeds_working_status();
        if embed {
            self.editor.set_working_status_indicator(Some(active));
        } else {
            self.ui.status.borrow_mut().add_child(active);
        }
        self.state.borrow_mut().active_status_embedded = embed;
    }

    /// An event of the custom editor, handled as the default editor would.
    pub(super) fn handle_custom_editor_event(&self, kind: &str, value: Option<String>) {
        match kind {
            "submit" => {
                self.editor.set_text("");
                self.handle_submit(value.as_deref().unwrap_or_default());
                let restored = self.editor.get_text();
                if !restored.is_empty() {
                    self.editor.set_text("");
                    self.set_extension_editor_text(&restored);
                }
            }
            "escape" => self.editor.trigger_escape(),
            "ctrlD" => self.editor.trigger_ctrl_d(),
            "pasteImage" => self.editor.trigger_paste_image(),
            "action" => self.editor.trigger_action(value.as_deref().unwrap_or_default()),
            _ => {}
        }
        self.request_render();
    }

    pub(super) fn set_working_message(&self, message: Option<String>) {
        self.extensions_state().working_message = message.clone();
        let active = self.state.borrow().active_status.clone();
        if let Some(active) = active
            && let Ok(mut active) = active.try_borrow_mut()
            && active.kind() == StatusKind::Working
        {
            active.set_message(message.as_deref().unwrap_or(DEFAULT_WORKING_MESSAGE));
        }
        self.request_render();
    }

    pub(super) fn set_working_visible(&self, visible: bool) {
        self.state.borrow_mut().working_visible = visible;
        if !visible {
            self.clear_status_indicator(Some(StatusKind::Working));
        } else if self.session().is_streaming() && self.active_status_kind() != Some(StatusKind::Working) {
            self.show_working_status_indicator();
        }
        self.request_render();
    }

    pub(super) fn set_working_indicator(&self, options: Option<WorkingIndicatorOptions>) {
        let options =
            options.map(|options| LoaderIndicatorOptions { frames: options.frames, interval_ms: options.interval_ms });
        self.extensions_state().working_indicator = options.clone();
        let active = self.state.borrow().active_status.clone();
        if let Some(active) = active
            && let Ok(mut active) = active.try_borrow_mut()
            && active.kind() == StatusKind::Working
        {
            active.set_indicator(options);
        }
        self.request_render();
    }

    pub(super) fn set_hidden_thinking_label(&self, label: Option<String>) {
        let label = label.unwrap_or_else(|| DEFAULT_HIDDEN_THINKING_LABEL.to_string());
        self.state.borrow_mut().hidden_thinking_label = label.clone();
        let mut messages = self.ui.chat.borrow().children.clone();
        messages.extend(self.state.borrow().streaming_component.clone().map(|component| component as ComponentRef));
        for child in &messages {
            if let Ok(mut child) = child.try_borrow_mut()
                && let Some(message) = child.as_any_mut().downcast_mut::<AssistantMessageComponent>()
            {
                message.set_hidden_thinking_label(&label);
            }
        }
        self.request_render();
    }

    /// The editor frame of the chat style; a caret or harbor box shows the placeholder for an idle
    /// or a `running` agent.
    pub(super) fn update_prompt_layout(&self, running: bool) {
        let prompt_layout = self.state.borrow().chat_style.prompt_layout;
        let prompt = || PromptBox {
            marker: Rc::new(|text: &str| theme().bold(&theme().fg(ThemeColor::Accent, text))),
            muted: Rc::new(|text: &str| theme().fg(ThemeColor::Muted, text)),
            placeholder: if running { RUNNING_PLACEHOLDER } else { IDLE_PLACEHOLDER }.to_string(),
        };
        let layout = match prompt_layout {
            Some(PromptLayout::Caret) => EditorLayout::Caret(prompt()),
            Some(PromptLayout::Harbor) => EditorLayout::Harbor(prompt()),
            Some(PromptLayout::Frame) | None => EditorLayout::Lines,
        };
        self.editor.editor().set_layout(layout);
    }

    /// Message prefixes, collapsed thinking, and the editor frame and border; rebuilds the chat.
    pub(super) fn set_chat_style(&self, style: ChatStyle) {
        let rounded = style.rounded_editor || style.prompt_layout == Some(PromptLayout::Frame);
        self.editor.editor().set_rounded(rounded);
        self.state.borrow_mut().chat_style = style.clone();
        self.update_prompt_layout(self.session().is_streaming());
        self.update_editor_border_color();
        self.place_status_indicator();
        let mut messages = self.ui.chat.borrow().children.clone();
        messages.extend(self.state.borrow().streaming_component.clone().map(|component| component as ComponentRef));
        for child in &messages {
            let Ok(mut child) = child.try_borrow_mut() else { continue };
            let any = child.as_any_mut();
            if let Some(message) = any.downcast_mut::<AssistantMessageComponent>() {
                message.set_chat_style(style.clone());
            } else if let Some(message) = any.downcast_mut::<UserMessageComponent>() {
                message.set_chat_style(style.clone());
            }
        }
        self.request_render();
    }

    pub(super) fn set_extension_theme(&self, name: &str) {
        let Some(controller) = self.theme_controller() else { return };
        let selection = crate::theme::ThemeSelection::Name(name.to_string());
        if controller.select(selection).is_ok() && self.settings().theme_setting().as_deref() != Some(name) {
            self.settings().set_theme(name);
        }
    }

    /// Route terminal input through `onTerminalInput` handlers while any is registered.
    pub(super) fn set_extension_terminal_input(&self, listening: bool) {
        let installed = self.extensions_state().terminal_listener.take();
        if let Some(id) = installed {
            self.tui().remove_input_listener(id);
        }
        if !listening {
            return;
        }
        let weak = self.weak.clone();
        let id = self.tui().add_input_listener(move |data| {
            let runner = weak.upgrade().and_then(|mode| mode.session().extension_runner());
            match runner.map(|runner| runner.terminal_input(data)) {
                Some(TerminalInputVerdict::Consume) => TuiInputListenerResult::Consume,
                Some(TerminalInputVerdict::Replace(data)) => TuiInputListenerResult::Replace(data),
                _ => TuiInputListenerResult::Pass,
            }
        });
        self.extensions_state().terminal_listener = Some(id);
    }

    /// Refresh what extensions read from other threads; runs on every UI loop pass.
    pub(super) fn sync_extension_mirror(&self) {
        let mirror = self.state.borrow().extensions.mirror.clone();
        if !self.has_custom_editor() {
            mirror.set_editor_text(&self.editor.get_expanded_text());
        }
        let expanded = self.state.borrow().tool_output_expanded;
        mirror.tools_expanded.store(expanded, std::sync::atomic::Ordering::SeqCst);
        let (columns, rows) = {
            let terminal = self.terminal.borrow();
            (terminal.columns(), terminal.rows())
        };
        // A render after a resize can run before this sync and read the old size.
        if mirror.set_size(columns, rows) {
            self.request_render();
        }
        let mut footer = self.footer_data.borrow_mut();
        let branch = footer.git_branch();
        mirror.set_footer(branch, footer.extension_statuses(), footer.available_provider_count());
        drop(footer);
        self.sync_run_footer(&mirror);
        self.sync_extension_overlays();
    }
}
