//! Startup dialogs shown before the session runtime exists (upstream `cli/startup-ui.ts`
//! `showFirstTimeSetup`, `cli/session-picker.ts`, the missing session cwd prompt of `main.ts`,
//! and the key wait of `showDeprecationWarnings`). Each runs inside a `LocalSet`.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use pi_tui::{Component, ComponentRef, ProcessTerminal, TerminalRef, Tui, TuiOptions, component, ui_loop};

use crate::interactive::startup_ui::show_startup_selector;
use crate::interactive_components::SessionSelector;
use crate::interactive_components::first_time_setup::{FirstTimeSetupComponent, apply_first_time_setup_result};
use crate::interactive_components::session_selector::{
    SessionSelectorOptions, all_sessions_loader, project_sessions_loader,
};
use crate::session::{SessionManager, session_dir};
use crate::settings::SettingsManager;
use crate::theme::{SYSTEM_THEME_NAME, get_terminal_theme, init_theme, resolve_theme_setting, set_theme};

/// The process terminal as a shared handle.
pub fn process_terminal() -> TerminalRef {
    Rc::new(RefCell::new(ProcessTerminal::new()))
}

/// A main-screen TUI for a startup dialog, themed by `settings`.
fn startup_tui(terminal: TerminalRef, settings: &SettingsManager) -> Tui {
    let setting = settings.theme_setting();
    let name = resolve_theme_setting(setting.as_deref(), get_terminal_theme());
    init_theme(Some(name.as_deref().unwrap_or(SYSTEM_THEME_NAME)), false);
    let options = TuiOptions { show_hardware_cursor: Some(settings.show_hardware_cursor()), ..TuiOptions::default() };
    let tui = Tui::new_main_screen(terminal, options);
    tui.set_clear_on_shrink(settings.clear_on_shrink());
    tui
}

/// Show `root` focused on `focus` until `done` holds a value, then clear and stop the TUI.
async fn run_until<T>(tui: &Tui, root: ComponentRef, focus: ComponentRef, done: &Rc<RefCell<Option<T>>>) -> Option<T> {
    tui.add_child(root);
    tui.set_focus(Some(focus));
    tui.start();
    tui.request_render(false);
    while done.borrow().is_none() {
        if ui_loop::run_once_async().await.is_err() {
            break;
        }
    }
    tui.clear();
    let _ = tui.render_now(false);
    tui.stop(pi_tui::TuiStopOptions::default());
    done.borrow_mut().take()
}

/// First-time setup on `terminal`: theme choice with live preview and analytics opt-in, saved
/// to `settings` unless cancelled. Returns whether a choice was saved.
pub async fn show_first_time_setup_on(terminal: TerminalRef, settings: &SettingsManager) -> bool {
    let tui = startup_tui(terminal, settings);
    let _ = set_theme(SYSTEM_THEME_NAME, false);
    let done: Rc<RefCell<Option<Option<_>>>> = Rc::default();
    let (submitted, cancelled, preview_tui) = (done.clone(), done.clone(), tui.clone());
    let setup = component(FirstTimeSetupComponent::new(
        move |name| {
            let _ = set_theme(name, false);
            preview_tui.invalidate();
            preview_tui.request_render(false);
        },
        move |result| *submitted.borrow_mut() = Some(Some(result)),
        move || *cancelled.borrow_mut() = Some(None),
    ));
    let result = run_until(&tui, setup.clone(), setup, &done).await.flatten();
    if let Some(result) = &result {
        apply_first_time_setup_result(settings, result);
    }
    result.is_some()
}

/// [`show_first_time_setup_on`] on the process terminal.
pub async fn show_first_time_setup(settings: &SettingsManager) -> bool {
    show_first_time_setup_on(process_terminal(), settings).await
}

/// Ask whether to continue a session whose stored cwd is gone in the current directory
/// (upstream `promptForMissingSessionCwd`). `prompt` is the missing cwd description.
pub async fn confirm_missing_session_cwd_on(terminal: TerminalRef, settings: &SettingsManager, prompt: &str) -> bool {
    let options = vec!["Continue".to_string(), "Cancel".to_string()];
    show_startup_selector(terminal, settings, prompt, options).await.as_deref() == Some("Continue")
}

/// What the startup session picker ended with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionPick {
    Selected(PathBuf),
    Cancelled,
    /// The exit key: upstream exits the process with code 0.
    Exit,
}

/// The `--resume` session picker (upstream `selectSession`): the current folder's sessions,
/// with search, all-projects scope, and delete. `dir_override` is the session directory setting.
pub async fn select_session_on(
    terminal: TerminalRef,
    settings: &SettingsManager,
    cwd: &Path,
    dir_override: Option<&Path>,
    agent_dir: &Path,
) -> SessionPick {
    crate::keybindings::KeybindingsManager::create(agent_dir).install();
    let tui = startup_tui(terminal, settings);
    let done: Rc<RefCell<Option<SessionPick>>> = Rc::default();
    let (select, cancel, exit) = (done.clone(), done.clone(), done.clone());
    let render = tui.clone();
    let custom = dir_override.filter(|dir| *dir != session_dir(agent_dir, cwd)).map(Path::to_path_buf);
    let selector = SessionSelector::new(
        project_sessions_loader(cwd.to_path_buf(), dir_override.map(Path::to_path_buf), agent_dir.to_path_buf()),
        all_sessions_loader(custom, agent_dir.to_path_buf()),
        move |path| *select.borrow_mut() = Some(SessionPick::Selected(path)),
        move || *cancel.borrow_mut() = Some(SessionPick::Cancelled),
        move || *exit.borrow_mut() = Some(SessionPick::Exit),
        move || render.request_render(false),
        SessionSelectorOptions {
            rename_session: Some(Box::new(|path, name| {
                SessionManager::open_with(&path, None, None)?.append_session_info(&name)?;
                Ok(())
            })),
            show_rename_hint: Some(false),
        },
        None,
    );
    let root: ComponentRef = selector.clone();
    run_until(&tui, root.clone(), root, &done).await.unwrap_or(SessionPick::Cancelled)
}

/// Captures the first key press.
struct AnyKey(Rc<RefCell<Option<()>>>);

impl Component for AnyKey {
    fn render(&mut self, _width: usize) -> Vec<String> {
        Vec::new()
    }

    fn handle_input(&mut self, _data: &str) {
        *self.0.borrow_mut() = Some(());
    }

    fn handles_input(&self) -> bool {
        true
    }
}

/// Wait for any key on `terminal`, as the deprecation notice asks.
pub async fn wait_for_any_key_on(terminal: TerminalRef) {
    let tui = Tui::new_main_screen(terminal, TuiOptions::default());
    let done: Rc<RefCell<Option<()>>> = Rc::default();
    let key: ComponentRef = component(AnyKey(done.clone()));
    run_until(&tui, key.clone(), key, &done).await;
}

#[cfg(test)]
#[path = "startup_dialogs_tests.rs"]
mod tests;
