//! Prompts shown before the session exists, such as the project trust question (upstream
//! `cli/startup-ui.ts` `showStartupSelector` and `core/project-trust.ts`).

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use pi_tui::{ProcessTerminal, TerminalRef, Tui, TuiOptions, component, ui_loop};

use crate::config::home_dir;
use crate::interactive_components::{ExtensionSelector, ExtensionSelectorOptions};
use crate::resources::trust::{
    DefaultProjectTrust, ProjectTrustRequest, ProjectTrustResolution, ProjectTrustStore, apply_project_trust_choice,
    resolve_project_trust,
};
use crate::settings::{SettingsManager, SettingsOptions};
use crate::theme::{get_terminal_theme, init_theme, resolve_theme_setting};

/// A selector on the main screen of `terminal`; `None` when dismissed. Runs inside a `LocalSet`.
pub async fn show_startup_selector(
    terminal: TerminalRef,
    settings: &SettingsManager,
    title: &str,
    options: Vec<String>,
) -> Option<String> {
    let setting = settings.theme_setting();
    init_theme(resolve_theme_setting(setting.as_deref(), get_terminal_theme()).as_deref(), false);
    let tui = Tui::new_main_screen(
        terminal,
        TuiOptions { show_hardware_cursor: Some(settings.show_hardware_cursor()), log_directory: None, env: None },
    );
    tui.set_clear_on_shrink(settings.clear_on_shrink());
    let answer: Rc<RefCell<Option<Option<String>>>> = Rc::default();
    let (selected, cancelled) = (answer.clone(), answer.clone());
    let selector = component(ExtensionSelector::new(
        title,
        options,
        move |option| *selected.borrow_mut() = Some(Some(option.to_string())),
        move || *cancelled.borrow_mut() = Some(None),
        ExtensionSelectorOptions { tui: Some(tui.clone()), ..Default::default() },
    ));
    tui.add_child(selector.clone());
    tui.set_focus(Some(selector));
    tui.start();
    tui.request_render(false);
    while answer.borrow().is_none() {
        if ui_loop::run_once_async().await.is_err() {
            break;
        }
    }
    tui.clear();
    let _ = tui.render_now(false);
    tui.stop(pi_tui::TuiStopOptions::default());
    answer.borrow_mut().take().flatten()
}

/// Asks whether to trust `cwd` when nothing else decides it, saving a remembered answer.
/// `None` when no prompt was needed; otherwise whether the project is trusted.
pub async fn prompt_project_trust(cwd: &Path, agent_dir: &Path, trust_override: Option<bool>) -> Option<bool> {
    let terminal: TerminalRef = Rc::new(RefCell::new(ProcessTerminal::new()));
    prompt_project_trust_on(terminal, cwd, agent_dir, trust_override).await
}

/// [`prompt_project_trust`] on `terminal`.
pub async fn prompt_project_trust_on(
    terminal: TerminalRef,
    cwd: &Path,
    agent_dir: &Path,
    trust_override: Option<bool>,
) -> Option<bool> {
    let global = SettingsManager::create(cwd, agent_dir, SettingsOptions { project_trusted: false });
    let store = ProjectTrustStore::new(agent_dir);
    let home = home_dir();
    let request = ProjectTrustRequest {
        cwd,
        home: &home,
        trust_override,
        default_project_trust: DefaultProjectTrust::parse(global.default_project_trust()),
        extension_decision: None,
        has_ui: true,
    };
    let Ok(ProjectTrustResolution::Prompt(prompt)) = resolve_project_trust(&store, &request) else { return None };
    let labels: Vec<String> = prompt.options.iter().map(|option| option.label.clone()).collect();
    let chosen = show_startup_selector(terminal, &global, &prompt.message, labels).await;
    let option = chosen.and_then(|label| prompt.options.iter().find(|option| option.label == label));
    Some(apply_project_trust_choice(&store, option).unwrap_or(false))
}
