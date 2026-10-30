//! Built-in slash commands that need no selector (upstream `setupEditorSubmitHandler` dispatch and
//! the `handle*Command` methods).

use pi_tui::{ComponentRef, Container, Markdown, Spacer, component};

use super::mode::InteractiveMode;
use super::selector::SelectorSpec;
use super::shutdown::joined;
use crate::agent_session::SessionImportFileNotFound;
use crate::interactive_components::DynamicBorder;
use crate::platform::changelog::{changelog_command_markdown, embedded_changelog_entries};
use crate::session::branching::MissingSessionCwd;
use crate::theme::{ThemeColor, theme};

/// The argument of `/<command> <argument>`, or `None` for another command.
fn argument<'a>(text: &'a str, command: &str) -> Option<Option<&'a str>> {
    let rest = text.strip_prefix('/')?.strip_prefix(command)?;
    if rest.is_empty() {
        return Some(None);
    }
    rest.starts_with(char::is_whitespace).then(|| Some(rest.trim()).filter(|arg| !arg.is_empty()))
}

/// The path of `/export <path>` or `/import <path>`: quoted, or up to the first whitespace.
pub(super) fn path_argument(text: &str, command: &str) -> Option<String> {
    let rest = text.strip_prefix(command)?;
    let rest = rest.strip_prefix(' ')?.trim_start();
    let first = rest.chars().next()?;
    if first == '"' || first == '\'' {
        let closing = rest[1..].find(first)?;
        return Some(rest[1..1 + closing].to_string());
    }
    Some(rest.split(char::is_whitespace).next().unwrap_or(rest).to_string())
}

impl InteractiveMode {
    /// Run a built-in command. Returns false for text that is not one.
    pub(super) fn run_slash_command(&self, text: &str) -> bool {
        if !text.starts_with('/') {
            return false;
        }
        let clear = || self.editor.set_text("");
        if text == "/settings" {
            clear();
            self.show_settings_selector();
        } else if text == "/extensions" {
            clear();
            self.handle_extensions_command();
        } else if text == "/scoped-models" {
            clear();
            self.show_models_selector();
        } else if let Some(search) = argument(text, "model") {
            clear();
            self.handle_model_command(search.map(str::to_string));
        } else if let Some(search) = argument(text, "thinking") {
            clear();
            self.handle_thinking_command(search.map(str::to_string));
        } else if argument(text, "export").is_some() {
            self.handle_export_command(text);
            clear();
        } else if argument(text, "import").is_some() {
            self.handle_import_command(text);
            clear();
        } else if text == "/share" {
            clear();
            self.handle_share_command();
        } else if let Some(hint) = argument(text, "bug") {
            clear();
            self.handle_bug_command(hint.map(str::to_string));
        } else if text == "/copy" {
            clear();
            self.handle_copy_command(false, false);
        } else if argument(text, "name").is_some() {
            self.handle_name_command(text);
            clear();
        } else if text == "/session" {
            self.handle_session_command();
            clear();
        } else if text == "/changelog" {
            self.handle_changelog_command();
            clear();
        } else if text == "/hotkeys" {
            self.add_bordered_markdown("Keyboard Shortcuts", &super::hotkeys::hotkeys_markdown());
            clear();
        } else if let Some(args) = argument(text, "diff") {
            self.handle_diff_command(args);
            clear();
        } else if text == "/context" {
            self.handle_context_command();
            clear();
        } else if let Some(provider) = argument(text, "usage") {
            clear();
            self.handle_usage_command(provider.map(str::to_string));
        } else if let Some(hint) = self.moved_feature_hint(text) {
            clear();
            self.show_warning(&hint);
        } else if text == "/fork" {
            clear();
            self.show_user_message_selector();
        } else if text == "/clone" {
            clear();
            self.handle_clone_command();
        } else if text == "/tree" {
            clear();
            self.show_tree_selector(None);
        } else if text == "/trust" {
            clear();
            self.show_trust_selector();
        } else if let Some(provider) = argument(text, "login") {
            clear();
            self.handle_login_command(provider.map(str::to_string));
        } else if text == "/logout" {
            clear();
            self.show_logout_selector();
        } else if text == "/new" {
            clear();
            self.handle_new_session();
        } else if let Some(instructions) = argument(text, "compact") {
            clear();
            self.handle_compact_command(instructions.map(str::to_string));
        } else if let Some(instructions) = argument(text, "init") {
            clear();
            self.handle_init_command(instructions);
        } else if let Some(instructions) = argument(text, "review") {
            clear();
            self.handle_review_command(instructions);
        } else if text == "/reload" {
            clear();
            self.handle_reload_command();
        } else if text == "/debug" {
            clear();
            self.defer(|mode| mode.handle_debug_command());
        } else if text == "/arminsayshi" {
            clear();
            if crate::interactive_components::play_armin_3d(&self.tui()) {
                return true;
            }
            self.add_spacer();
            self.add_to_chat(component(crate::interactive_components::armin::ArminComponent::new(&self.tui())));
        } else if text == "/dementedelves" {
            clear();
            self.add_spacer();
            let announcement = crate::interactive_components::EarendilAnnouncementComponent::new();
            self.add_to_chat(component(announcement));
        } else if text == "/resume" {
            clear();
            self.show_session_selector();
        } else if text == "/undo" {
            clear();
            self.handle_undo_command();
        } else if text == "/retry" {
            clear();
            self.handle_retry_command();
        } else if let Some(args) = argument(text, "mcp")
            && self.session().builtin_extension_enabled("mcp")
        {
            clear();
            self.handle_mcp_command(args.map(str::to_string));
        } else if text == "/llama" && self.session().builtin_extension_enabled(crate::llama::LLAMA_PROVIDER_ID) {
            clear();
            self.handle_llama_command();
        } else if text == "/quit" {
            clear();
            self.request_shutdown();
        } else {
            return false;
        }
        self.request_render();
        true
    }

    fn handle_export_command(&self, text: &str) {
        let path = path_argument(text, "/export");
        let session = self.session();
        let result = match path.as_deref() {
            Some(path) if path.ends_with(".jsonl") => session.export_to_jsonl(Some(std::path::Path::new(path))),
            Some(path) if crate::export::is_markdown_path(path) => session.export_to_markdown(path),
            path => session.export_to_html(path),
        };
        match result {
            Ok(file) => self.show_status(&format!("Session exported to: {}", file.display())),
            Err(error) => self.show_error(&format!("Failed to export session: {error}")),
        }
    }

    fn handle_import_command(&self, text: &str) {
        let Some(path) = path_argument(text, "/import") else {
            self.show_error("Usage: /import <path.jsonl>");
            return;
        };
        let message = format!("Replace current session with {path}?");
        self.show_confirm("Import session", &message, move |mode, confirmed| {
            if !confirmed {
                mode.show_status("Import cancelled");
                return;
            }
            mode.clear_status_indicator(None);
            let path = path.clone();
            mode.spawn(move |mode| async move { mode.import_session(path).await });
        });
    }

    async fn import_session(&self, path: String) {
        let input = std::path::PathBuf::from(&path);
        let mut cwd_override: Option<std::path::PathBuf> = None;
        loop {
            let (runtime, input, cwd) = (self.runtime.clone(), input.clone(), cwd_override.clone());
            let import = async move { runtime.import_from_jsonl(&input, cwd.as_deref()).await };
            let error = match joined(tokio::spawn(import).await) {
                Ok(result) if result.cancelled => return self.show_status("Import cancelled"),
                Ok(_) => return self.show_status(&format!("Session imported from: {path}")),
                Err(error) => error,
            };
            if let Some(missing) = error.downcast_ref::<MissingSessionCwd>().filter(|_| cwd_override.is_none()) {
                if !self.ask_confirm("Session cwd not found", &missing.prompt()).await {
                    return self.show_status("Import cancelled");
                }
                cwd_override = Some(missing.fallback_cwd.clone());
                continue;
            }
            if error.is::<SessionImportFileNotFound>() {
                return self.show_error(&format!("Failed to import session: {error}"));
            }
            return self.handle_fatal_runtime_error("Failed to import session", &error);
        }
    }

    /// `/copy` and the copy key: the selection in fullscreen when not copied on select, else
    /// the last assistant message.
    pub(super) fn handle_copy_command(&self, flash: bool, prefer_selection: bool) {
        let tui = self.tui();
        if prefer_selection && tui.is_viewport() && !tui.get_copy_on_select() && tui.has_active_selection() {
            drop(tui.copy_active_selection_to_clipboard());
            return;
        }
        let Some(text) = self.session().last_assistant_text() else {
            self.show_error("No agent messages to copy yet.");
            return;
        };
        match crate::clipboard::copy_to_clipboard(&text) {
            Ok(()) if flash && tui.is_viewport() => tui.flash("Copied!", None),
            Ok(()) => self.show_status("Copied last agent message to clipboard"),
            Err(error) => self.show_error(&error.to_string()),
        }
    }

    fn handle_name_command(&self, text: &str) {
        let name = text.strip_prefix("/name").unwrap_or("").trim();
        let session = self.session();
        if name.is_empty() {
            match session.session_name() {
                Some(current) => {
                    self.add_spacer();
                    let line = format!("Session name: {current}");
                    self.add_text(move || theme().fg(ThemeColor::Dim, &line), 1, 0);
                }
                None => self.show_warning("Usage: /name <name>"),
            }
            return;
        }
        if let Err(error) = session.set_session_name(name) {
            self.show_error(&error.to_string());
            return;
        }
        let stored = session.session_name();
        if stored.as_deref() != Some(name) {
            let normalized = serde_json::Value::from(stored.clone().unwrap_or_default());
            let original = serde_json::Value::from(name);
            self.show_warning(&format!("Session name was normalized from {original} to {normalized}"));
        }
        let line = format!("Session name set: {}", stored.unwrap_or_else(|| name.to_string()));
        self.add_spacer();
        self.add_text(move || theme().fg(ThemeColor::Dim, &line), 1, 0);
    }

    pub(super) fn add_bordered_markdown(&self, title: &'static str, markdown: &str) {
        self.add_spacer();
        self.add_to_chat(component(DynamicBorder::new()));
        self.add_text(move || theme().bold(&theme().fg(ThemeColor::Accent, title)), 1, 0);
        self.add_spacer();
        let options = self.render_options();
        let markdown = Markdown::new(markdown, 1, 1, options.markdown_theme, None, Default::default());
        self.add_to_chat(component(markdown));
        self.add_to_chat(component(DynamicBorder::new()));
        self.request_render();
    }

    fn handle_changelog_command(&self) {
        let entries = embedded_changelog_entries();
        let markdown = if entries.is_empty() {
            "No changelog entries found.".to_string()
        } else {
            changelog_command_markdown(&entries)
        };
        self.add_bordered_markdown("What's New", &markdown);
    }

    /// `/new` and its key: replace the session with an empty one.
    pub(super) fn handle_new_session(&self) {
        self.clear_status_indicator(None);
        let runtime = self.runtime.clone();
        self.spawn(move |mode| async move {
            match joined(tokio::spawn(async move { runtime.new_session(None, None).await }).await) {
                Ok(result) if result.cancelled => {}
                Ok(_) => {
                    mode.add_spacer();
                    mode.add_text(|| theme().fg(ThemeColor::Accent, "✓ New session started"), 1, 1);
                    mode.request_render();
                }
                Err(error) => mode.handle_fatal_runtime_error("Failed to create session", &error),
            }
        });
    }

    fn handle_compact_command(&self, instructions: Option<String>) {
        self.clear_status_indicator(None);
        let session = self.session();
        tokio::spawn(async move {
            let _ = session.compact(instructions.as_deref()).await;
        });
    }

    /// Put `component` in place of the editor until the editor is restored.
    pub(super) fn show_in_editor_slot(&self, component: ComponentRef, focus: ComponentRef) {
        self.selector.show(Box::new(move |_done| SelectorSpec::new(component).with_focus(focus)));
        self.request_render();
    }

    /// `/reload`: settings, keybindings, skills, prompts, themes, and context files.
    pub(super) fn handle_reload_command(&self) {
        let session = self.session();
        if session.is_streaming() {
            return self.show_warning("Wait for the current response to finish before reloading.");
        }
        if session.is_compacting() {
            return self.show_warning("Wait for compaction to finish before reloading.");
        }
        self.reset_extension_ui();
        let mut reload_box = Container::new();
        let border = || component(DynamicBorder::with_color(|text: &str| theme().fg(ThemeColor::Border, text)));
        let message = "Reloading keybindings, extensions, skills, prompts, themes, and context files...";
        reload_box.add_child(border());
        reload_box.add_child(component(Spacer::new(1)));
        let text = super::components::ThemedText::new(move || theme().fg(ThemeColor::Muted, message), 1, 0);
        reload_box.add_child(component(text));
        reload_box.add_child(component(Spacer::new(1)));
        reload_box.add_child(border());
        let reload_box: ComponentRef = component(reload_box);
        self.show_in_editor_slot(reload_box.clone(), reload_box);
        self.request_full_render();
        self.spawn(|mode| async move {
            tokio::task::yield_now().await;
            let session = mode.session();
            let error = match tokio::spawn(async move { session.reload_extensions().await }).await {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(format!("{error:#}")),
                Err(error) => Some(error.to_string()),
            };
            if let Some(error) = error {
                mode.selector.restore_editor();
                return mode.show_error(&format!("Reload failed: {error}"));
            }
            {
                let settings = mode.settings();
                let mut state = mode.state.borrow_mut();
                state.hide_thinking_block = settings.hide_thinking_block();
                state.output_pad = usize::from(settings.output_pad());
            }
            mode.rebuild_chat_from_messages();
            crate::keybindings::reload_installed_keybindings(&mode.agent_dir);
            let expanded = mode.state.borrow().tool_output_expanded;
            let header = mode.state.borrow().header.clone();
            if let Some(header) = header {
                header.borrow_mut().set_expanded(expanded);
            }
            mode.register_resource_themes();
            mode.apply_runtime_settings();
            if let Some(controller) = mode.theme_controller() {
                controller.apply_from_settings();
            }
            mode.after_extensions_bound();
            mode.show_loaded_resources(false, true);
            if let Some(error) = mode.session().model_runtime().load_error() {
                mode.show_error(&format!("models.json error: {error}"));
            }
            let cwd = mode.runtime.cwd();
            let trusted = mode.settings().is_project_trusted();
            let home = crate::config::home_dir();
            let saved = {
                let auto_cwd = &mut mode.options.borrow_mut().auto_trust_on_reload_cwd;
                crate::startup::save_implicit_project_trust_after_reload(
                    auto_cwd,
                    &cwd,
                    trusted,
                    &mode.agent_dir,
                    &home,
                )
            };
            match saved {
                Ok(saved) => mode.show_status(crate::startup::reload_status(saved)),
                Err(warning) => {
                    mode.show_warning(&warning);
                    mode.show_status(crate::startup::reload_status(false));
                }
            }
            mode.selector.restore_editor();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_command_arguments() {
        assert_eq!(argument("/model", "model"), Some(None));
        assert_eq!(argument("/model  sonnet ", "model"), Some(Some("sonnet")));
        assert_eq!(argument("/models", "model"), None);
        assert_eq!(argument("model", "model"), None);
    }

    #[test]
    fn parses_path_arguments() {
        assert_eq!(path_argument("/export", "/export"), None);
        assert_eq!(path_argument("/export out.html extra", "/export"), Some("out.html".into()));
        assert_eq!(path_argument("/export \"my file.html\"", "/export"), Some("my file.html".into()));
        assert_eq!(path_argument("/export 'unterminated", "/export"), None);
    }
}
