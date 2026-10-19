//! Startup, session rebinding, runtime settings, and shutdown (upstream `init`, `run` warnings,
//! `rebindCurrentSession`, `applyRuntimeSettings`, `shutdown`, and `stop`).

use std::rc::Rc;
use std::time::Duration;

use pi_tui::{Spacer, Text, component};

use super::components::ExpandableText;
use super::components::pi_logo::{pi_logo_lines, pi_wordmark, supports_pi_logo};
use super::mode::InteractiveMode;
use crate::interactive_components::{DynamicBorder, key_hint, key_text, raw_key_hint};
use crate::platform::changelog::condensed_changelog_notice;
use crate::settings::QuietStartup;
use crate::settings::schema::{FullscreenExitOutput, TuiMode};
use crate::theme::{ThemeColor, load_resource_themes, set_registered_themes, theme};

pub(super) const VERSION: &str = env!("CARGO_PKG_VERSION");

impl InteractiveMode {
    pub(super) fn verbose(&self) -> bool {
        self.options.borrow().verbose
    }

    fn should_show_startup_header(&self) -> bool {
        self.verbose() || self.settings().quiet_startup() != QuietStartup::On
    }

    pub(super) fn should_show_startup_details(&self) -> bool {
        self.verbose() || self.settings().quiet_startup() == QuietStartup::Off
    }

    pub(super) fn startup_expanded(&self) -> bool {
        self.verbose() || self.state.borrow().tool_output_expanded
    }

    /// Mount the UI, start the terminal, and replay the session (upstream `init`).
    pub async fn init(self: &Rc<Self>) {
        if self.state.borrow().initialized {
            return;
        }
        let changelog = self.options.borrow_mut().changelog_markdown.take();
        self.state.borrow_mut().changelog_markdown = changelog;
        self.print_model_scope();
        self.render_widgets();
        self.mount_layout();
        self.setup_startup_handlers();
        if self.uses_process_terminal {
            self.install_aws_mfa_prompt();
        }
        let tui = self.tui();
        tui.set_focus(Some(self.selector.editor()));
        tui.start();
        self.state.borrow_mut().initialized = true;
        self.report_program_status();
        pi_tui::components::image::set_image_transcoder(Some(std::sync::Arc::new(|data: &str, mime: &str| {
            crate::images::convert_to_png(data, mime).map(|(png, _)| png)
        })));
        self.register_resource_themes();
        if let Some(controller) = self.theme_controller() {
            controller.apply_from_settings();
            let wait = controller.wait_for_terminal_colors();
            self.pump_until(wait, Duration::from_millis(500)).await;
        }
        self.build_header();
        self.show_managed_tool_warnings();
        self.setup_key_handlers();
        self.setup_autocomplete_provider();
        self.rebind_current_session();
        let bridge = std::sync::Arc::downgrade(&self.bridge);
        crate::theme::on_theme_change(move || {
            if let Some(bridge) = bridge.upgrade() {
                bridge.push(super::bridge::UiEvent::ThemeChanged);
            }
        });
        let weak = self.weak.clone();
        FooterDataWatch::start(self, move || {
            if let Some(mode) = weak.upgrade() {
                mode.request_render();
            }
        });
        self.request_render();
    }

    /// Upstream `ensureTool("fd")` and `ensureTool("rg")` statuses: on Termux, how to install a
    /// missing helper tool.
    fn show_managed_tool_warnings(&self) {
        let env = crate::platform::PlatformEnv::from_process();
        let bin_dir = self.agent_dir.join("bin");
        let path = pi_ai::env::process_env_value("PATH").unwrap_or_default();
        let available = |command: &str| {
            let mut dirs = std::iter::once(bin_dir.clone()).chain(std::env::split_paths(&path));
            dirs.any(|dir| dir.join(command).is_file())
        };
        let warnings = crate::platform::termux_tool_warnings(&env, available);
        if warnings.is_empty() {
            return;
        }
        self.add_to_chat(component(Spacer::new(1)));
        for warning in warnings {
            let message = format!("Warning: {warning}");
            self.add_text(move || theme().fg(ThemeColor::Warning, &message), 1, 0);
        }
        self.request_render();
    }

    /// Run the UI loop until `future` completes or `limit` passes.
    async fn pump_until(&self, future: impl Future<Output = ()>, limit: Duration) {
        let deadline = tokio::time::Instant::now() + limit;
        tokio::pin!(future);
        loop {
            tokio::select! {
                _ = &mut future => return,
                _ = tokio::time::sleep_until(deadline) => return,
                result = self.run_once() => if result.is_err() { return },
            }
        }
    }

    fn print_model_scope(&self) {
        let scoped = self.session().scoped_models();
        if scoped.is_empty() || !self.should_show_startup_details() || !self.uses_process_terminal {
            return;
        }
        let list: Vec<String> = scoped
            .iter()
            .map(|scoped| match scoped.thinking_level {
                Some(level) => format!("{}:{}", scoped.model.id, level.as_str()),
                None => scoped.model.id.clone(),
            })
            .collect();
        let keys = pi_tui::get_keybindings().get_keys("app.model.cycleForward");
        let hint = if keys.is_empty() {
            String::new()
        } else {
            let text = crate::interactive_components::format_key_display_text(&keys.join("/"));
            theme().fg(ThemeColor::Muted, &format!(" ({text} to cycle)"))
        };
        println!("{}", theme().fg(ThemeColor::Dim, &format!("Model scope: {}{hint}", list.join(", "))));
    }

    fn build_header(&self) {
        let mut header = self.ui.header.borrow_mut();
        if !self.should_show_startup_header() {
            header.add_child(component(Text::new("", 0, 0)));
            return;
        }
        let show_details = self.should_show_startup_details();
        let show_logo = supports_pi_logo();
        let with_logo = move |hints: String| {
            let version = theme().fg(ThemeColor::Dim, &format!("v{VERSION}"));
            if !show_logo {
                return format!("{} {version}\n{hints}", pi_wordmark());
            }
            let [top, bottom] = pi_logo_lines();
            format!("{top} {version}\n{bottom} {hints}")
        };
        let collapsed = {
            move || {
                let compact = [
                    key_hint("app.interrupt", "interrupt"),
                    raw_key_hint(&format!("{}/{}", key_text("app.clear"), key_text("app.exit")), "clear/exit"),
                    raw_key_hint("/", "commands"),
                    raw_key_hint("!", "bash"),
                    key_hint("app.tools.expand", "more"),
                ]
                .join(&theme().fg(ThemeColor::Muted, " · "));
                let resources = if show_details { " and loaded resources" } else { "" };
                let onboarding = theme().fg(
                    ThemeColor::Dim,
                    &format!("Press {} to show full startup help{resources}.", key_text("app.tools.expand")),
                );
                format!("{}\n{onboarding}\n\n{}", with_logo(compact), onboarding_line())
            }
        };
        let expanded = move || format!("{}\n\n{}", with_logo(expanded_instructions()), onboarding_line());
        let header_text = Rc::new(std::cell::RefCell::new(ExpandableText::new(
            Box::new(collapsed),
            Box::new(expanded),
            self.startup_expanded(),
            1,
            0,
        )));
        if show_logo {
            let weak = self.weak.clone();
            header_text.borrow_mut().set_on_logo_click(Some(Box::new(move |column, row| {
                if let Some(mode) = weak.upgrade() {
                    let (column, row) = (column.max(0) as usize, row.max(0) as usize);
                    crate::interactive_components::play_pi_logo_3d(&mode.tui(), column, row);
                }
            })));
        }
        header.add_child(component(Spacer::new(1)));
        header.add_child(header_text.clone());
        header.add_child(component(Spacer::new(1)));
        drop(header);
        self.state.borrow_mut().header = Some(header_text);
    }

    /// The changelog notice above the first message, once.
    pub(super) fn show_startup_notices(&self) {
        let markdown = {
            let mut state = self.state.borrow_mut();
            if std::mem::replace(&mut state.startup_notices_shown, true) {
                return;
            }
            state.changelog_markdown.clone()
        };
        let Some(markdown) = markdown else { return };
        if !self.ui.chat.borrow().children.is_empty() {
            self.add_spacer();
        }
        self.add_to_chat(component(DynamicBorder::new()));
        if self.settings().collapse_changelog() {
            let notice =
                condensed_changelog_notice(&markdown, VERSION).replace("/changelog", &theme().bold("/changelog"));
            self.add_to_chat(component(Text::new(&notice, 1, 0)));
        } else {
            self.add_text(|| theme().bold(&theme().fg(ThemeColor::Accent, "What's New")), 1, 0);
            self.add_spacer();
            let options = self.render_options();
            let view = pi_tui::Markdown::new(markdown.trim(), 1, 0, options.markdown_theme, None, Default::default());
            self.add_to_chat(component(view));
            self.add_spacer();
        }
        self.add_to_chat(component(DynamicBorder::new()));
    }

    /// Startup diagnostics, models.json errors, model fallback, and crash notices.
    pub(super) fn show_startup_warnings(&self) {
        let diagnostics = std::mem::take(&mut self.options.borrow_mut().startup_diagnostics);
        for diagnostic in diagnostics {
            match diagnostic.level {
                crate::agent_session::DiagnosticLevel::Error => self.show_error(&diagnostic.message),
                crate::agent_session::DiagnosticLevel::Warning => self.show_warning(&diagnostic.message),
                crate::agent_session::DiagnosticLevel::Info => self.show_status(&diagnostic.message),
            }
        }
        if let Some(error) = self.session().model_runtime().load_error() {
            self.show_error(&format!("models.json error: {error}"));
        }
        if let Some(message) = self.options.borrow_mut().model_fallback_message.take() {
            self.show_warning(&message);
        }
        let crashes = crate::diagnostics::crash_log::crash_log_path(&self.agent_dir);
        let now = jiff::Timestamp::now().as_millisecond();
        if let Some(crash) = crate::diagnostics::crash_log::take_unnotified_crash(&crashes, now) {
            let app = crate::resources::brand::APP_NAME;
            self.show_warning(&format!(
                "{app} crashed on {} ({}). Run /bug to report it; the crash details are attached automatically.",
                crash.timestamp, crash.message
            ));
        }
        self.start_version_check();
        self.check_tmux_keyboard_setup();
        self.maybe_warn_about_anthropic_subscription_auth(None);
    }

    pub(super) fn register_resource_themes(&self) {
        let loaded = {
            let services = self.session().services().clone();
            let loader = services.resources();
            load_resource_themes(loader.resources(), None)
        };
        let _ = set_registered_themes(loaded.themes);
    }

    /// Settings that apply to the UI right away (upstream `applyRuntimeSettings`).
    pub(super) fn apply_runtime_settings(&self) {
        let settings = self.settings();
        let session = self.session();
        super::layout::apply_capability_overrides(&settings);
        self.ui.transcript.set_scrollbar(super::layout::scrollbar_setting(settings.fullscreen_scrollbar()));
        let tui = self.tui();
        if tui.is_viewport() {
            tui.set_copy_on_select(settings.fullscreen_copy_on_select());
            tui.set_wheel_scroll_lines(super::layout::wheel_lines(&settings));
        }
        {
            let mut footer = self.footer.borrow_mut();
            footer.set_session(session.clone());
            footer.set_auto_compact_enabled(session.auto_compaction_enabled());
        }
        self.footer_data.borrow_mut().set_cwd(session.cwd());
        {
            let mut state = self.state.borrow_mut();
            state.hide_thinking_block = settings.hide_thinking_block();
            state.output_pad = usize::from(settings.output_pad());
        }
        tui.set_show_hardware_cursor(settings.show_hardware_cursor());
        let clear_on_shrink = settings.clear_on_shrink();
        tui.set_clear_on_shrink(clear_on_shrink);
        if !clear_on_shrink && self.state.borrow().active_status.is_none() {
            self.ui.status.borrow_mut().clear();
        }
        self.editor.editor().set_padding_x(settings.editor_padding_x() as usize);
        self.editor.editor().set_autocomplete_max_visible(settings.autocomplete_max_visible() as usize);
    }

    /// Follow the current session (startup and after the runtime replaced it).
    pub(super) fn rebind_current_session(&self) {
        let session = self.session();
        let first_bind = !self.state.borrow().startup_notices_shown;
        self.program_status.borrow_mut().reset();
        self.apply_runtime_settings();
        self.bridge.subscribe(&session);
        self.install_session_hooks();
        self.refresh_login_providers();
        if first_bind {
            self.show_loaded_resources(false, false);
            self.render_initial_messages();
            self.show_startup_notices();
        } else {
            self.render_current_session_state();
            if let Some(controller) = self.theme_controller() {
                controller.apply_from_settings();
            }
        }
        self.update_available_provider_count();
        self.update_editor_border_color();
        self.update_terminal_title();
        self.report_program_status();
        if first_bind {
            self.bind_current_extensions();
        } else {
            self.after_extensions_bound();
        }
    }

    pub(super) fn update_available_provider_count(&self) {
        let count = {
            let session = self.session();
            let models = session.model_runtime().available_models();
            let mut providers: Vec<&str> = models.iter().map(|model| model.provider.as_str()).collect();
            providers.sort_unstable();
            providers.dedup();
            providers.len()
        };
        self.footer_data.borrow_mut().set_available_provider_count(count);
    }

    /// Quit after the current dispatch: stop the TUI, dispose of the runtime, and leave the loop.
    pub(super) fn request_shutdown(&self) {
        if std::mem::replace(&mut self.state.borrow_mut().shutting_down, true) {
            return;
        }
        self.spawn(|mode| async move {
            if let Some(controller) = mode.theme_controller() {
                controller.disable_auto_sync();
            }
            mode.terminal.borrow_mut().drain_input(Duration::from_millis(1000), Duration::from_millis(50));
            mode.stop(None);
            let runtime = mode.runtime.clone();
            let _ = tokio::spawn(async move { runtime.dispose().await }).await;
            if mode.uses_process_terminal
                && let Some(command) = super::util::resume_command(&mode.session(), &mode.agent_dir)
            {
                let label = theme().fg(ThemeColor::Dim, "To resume this session:");
                println!("{label} {command}");
            }
            mode.state.borrow_mut().exit = true;
            mode.bridge.waker().wake();
        });
    }

    /// Restore the terminal (upstream `stop`). In fullscreen with `fullscreenExitOutput:
    /// transcript`, the transcript is printed to the main screen first.
    pub fn stop(&self, exit_output: Option<FullscreenExitOutput>) {
        if self.uses_process_terminal {
            self.remove_aws_mfa_prompt();
        }
        self.selector.dispose_active();
        self.set_terminal_progress(false);
        self.clear_status_indicator(None);
        if let Some(controller) = self.theme_controller() {
            controller.disable_auto_sync();
        }
        self.footer_data.borrow_mut().dispose();
        self.bridge.unsubscribe();
        if !std::mem::replace(&mut self.state.borrow_mut().initialized, false) {
            return;
        }
        let exit_output = exit_output.unwrap_or_else(|| self.settings().fullscreen_exit_output());
        let fullscreen = self.tui().is_viewport();
        if fullscreen && exit_output == FullscreenExitOutput::Transcript {
            let tui = self.tui();
            while tui.has_overlay() {
                tui.hide_overlay();
            }
            self.switch_tui_mode(TuiMode::Regular, false, false);
            let _ = self.tui().render_now(false);
        }
        let preserve_screen = self.tui().is_viewport();
        self.tui().stop(pi_tui::TuiStopOptions { preserve_screen });
    }
}

fn onboarding_line() -> String {
    theme().fg(ThemeColor::Dim, "Pi can explain its own features and look up its docs. Ask it how to use or extend Pi.")
}

fn expanded_instructions() -> String {
    [
        key_hint("app.interrupt", "to interrupt"),
        key_hint("app.clear", "to clear"),
        raw_key_hint(&format!("{} twice", key_text("app.clear")), "to exit"),
        key_hint("app.exit", "to exit (empty)"),
        key_hint("app.suspend", "to suspend"),
        key_hint("tui.editor.deleteToLineEnd", "to delete to end"),
        key_hint("app.thinking.cycle", "to cycle thinking level"),
        raw_key_hint(
            &format!("{}/{}", key_text("app.model.cycleForward"), key_text("app.model.cycleBackward")),
            "to cycle models",
        ),
        key_hint("app.model.select", "to select model"),
        key_hint("app.tools.expand", "to expand tools"),
        key_hint("app.thinking.toggle", "to expand thinking"),
        key_hint("app.editor.external", "for external editor"),
        raw_key_hint("/", "for commands"),
        raw_key_hint("!", "to run bash"),
        raw_key_hint("!!", "to run bash (no context)"),
        key_hint("app.message.followUp", "to queue follow-up"),
        key_hint("app.message.dequeue", "to edit all queued messages"),
        key_hint("app.clipboard.pasteImage", "to paste files on macOS, images, or text"),
        raw_key_hint("drop files", "to attach"),
    ]
    .join("\n")
}

/// Polls the git branch for the footer.
struct FooterDataWatch;

impl FooterDataWatch {
    fn start(mode: &InteractiveMode, on_change: impl Fn() + 'static) {
        super::footer_data::FooterData::watch(&mode.footer_data, on_change);
    }
}
