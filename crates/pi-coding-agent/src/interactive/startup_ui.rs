//! Prompts shown before the session exists, such as the project trust question (upstream
//! `cli/startup-ui.ts` `showStartupSelector` and `core/project-trust.ts`).

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use async_trait::async_trait;
use pi_tui::{ProcessTerminal, TerminalRef, Tui, TuiOptions, component, ui_loop};
use tokio::sync::{mpsc, oneshot};

use crate::config::home_dir;
use crate::extension_ui::{DialogOptions, ExtensionUi, NotifyType, WidgetPlacement};
use crate::interactive_components::{
    ExtensionInput, ExtensionInputOptions, ExtensionSelector, ExtensionSelectorOptions,
};
use crate::resources::trust::{
    DefaultProjectTrust, ExtensionTrustDecision, ProjectTrustRequest, ProjectTrustResolution, ProjectTrustStore,
    apply_project_trust_choice, resolve_project_trust,
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
    let title = title.to_string();
    show_startup_dialog(terminal, settings, move |tui, answer| {
        let (selected, cancelled) = (answer.clone(), answer);
        component(ExtensionSelector::new(
            &title,
            options,
            move |option| *selected.borrow_mut() = Some(Some(option.to_string())),
            move || *cancelled.borrow_mut() = Some(None),
            ExtensionSelectorOptions { tui: Some(tui), ..Default::default() },
        ))
    })
    .await
}

/// A one-line input on the main screen of `terminal` (upstream `showStartupInput`).
pub async fn show_startup_input(terminal: TerminalRef, settings: &SettingsManager, title: &str) -> Option<String> {
    let title = title.to_string();
    show_startup_dialog(terminal, settings, move |tui, answer| {
        let (submitted, cancelled) = (answer.clone(), answer);
        component(ExtensionInput::new(
            &title,
            None,
            move |value| *submitted.borrow_mut() = Some(Some(value.to_string())),
            move || *cancelled.borrow_mut() = Some(None),
            ExtensionInputOptions { tui: Some(tui), timeout: None, initial_value: None, description: None },
        ))
    })
    .await
}

type Answer = Rc<RefCell<Option<Option<String>>>>;

async fn show_startup_dialog<C: pi_tui::Component + 'static>(
    terminal: TerminalRef,
    settings: &SettingsManager,
    create: impl FnOnce(Tui, Answer) -> Rc<RefCell<C>>,
) -> Option<String> {
    let setting = settings.theme_setting();
    init_theme(resolve_theme_setting(setting.as_deref(), get_terminal_theme()).as_deref(), false);
    let tui = Tui::new_main_screen(
        terminal,
        TuiOptions { show_hardware_cursor: Some(settings.show_hardware_cursor()), log_directory: None, env: None },
    );
    tui.set_clear_on_shrink(settings.clear_on_shrink());
    let answer: Answer = Rc::default();
    let dialog: pi_tui::ComponentRef = create(tui.clone(), answer.clone());
    tui.add_child(dialog.clone());
    tui.set_focus(Some(dialog));
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
    prompt_project_trust_with(terminal, cwd, agent_dir, trust_override, None).await
}

/// [`prompt_project_trust_on`] after the `project_trust` handlers of extensions: their decision
/// counts as the answer. `None` when neither they nor a prompt decided.
pub async fn prompt_project_trust_with(
    terminal: TerminalRef,
    cwd: &Path,
    agent_dir: &Path,
    trust_override: Option<bool>,
    extension_decision: Option<ExtensionTrustDecision>,
) -> Option<bool> {
    let global = SettingsManager::create(cwd, agent_dir, SettingsOptions { project_trusted: false });
    let store = ProjectTrustStore::new(agent_dir);
    let home = home_dir();
    let request = ProjectTrustRequest {
        cwd,
        home: &home,
        trust_override,
        default_project_trust: DefaultProjectTrust::parse(global.default_project_trust()),
        extension_decision,
        has_ui: true,
    };
    let prompt = match resolve_project_trust(&store, &request) {
        Ok(ProjectTrustResolution::Prompt(prompt)) => prompt,
        Ok(ProjectTrustResolution::Decided(trusted)) if extension_decision.is_some() => return Some(trusted),
        _ => return None,
    };
    let labels: Vec<String> = prompt.options.iter().map(|option| option.label.clone()).collect();
    let chosen = show_startup_selector(terminal, &global, &prompt.message, labels).await;
    let option = chosen.and_then(|label| prompt.options.iter().find(|option| option.label == label));
    Some(apply_project_trust_choice(&store, option).unwrap_or(false))
}

/// A dialog a `project_trust` handler asked for before the session exists.
enum StartupRequest {
    Select { title: String, options: Vec<String>, reply: oneshot::Sender<Option<String>> },
    Input { title: String, reply: oneshot::Sender<Option<String>> },
}

/// `ctx.ui` of `project_trust` handlers at startup (upstream `createProjectTrustContext`):
/// select, confirm, and input show startup dialogs; the other calls do nothing.
struct StartupExtensionUi {
    requests: mpsc::UnboundedSender<StartupRequest>,
}

impl StartupExtensionUi {
    async fn ask(&self, request: impl FnOnce(oneshot::Sender<Option<String>>) -> StartupRequest) -> Option<String> {
        let (reply, answer) = oneshot::channel();
        self.requests.send(request(reply)).ok()?;
        answer.await.ok().flatten()
    }
}

#[async_trait]
impl ExtensionUi for StartupExtensionUi {
    async fn select(&self, title: &str, options: &[String], _opts: DialogOptions) -> Option<String> {
        let (title, options) = (title.to_string(), options.to_vec());
        self.ask(|reply| StartupRequest::Select { title, options, reply }).await
    }

    async fn confirm(&self, title: &str, message: &str, _opts: DialogOptions) -> bool {
        let (title, options) = (format!("{title}\n{message}"), vec!["Yes".to_string(), "No".to_string()]);
        self.ask(|reply| StartupRequest::Select { title, options, reply }).await.as_deref() == Some("Yes")
    }

    async fn input(&self, title: &str, _placeholder: Option<&str>, _opts: DialogOptions) -> Option<String> {
        let title = title.to_string();
        self.ask(|reply| StartupRequest::Input { title, reply }).await
    }

    async fn editor(&self, _title: &str, _prefill: Option<&str>, _opts: DialogOptions) -> Option<String> {
        None
    }

    fn notify(&self, _message: &str, _kind: Option<NotifyType>) {}

    fn set_status(&self, _key: &str, _text: Option<&str>) {}

    fn set_widget(&self, _key: &str, _lines: Option<&[String]>, _placement: Option<WidgetPlacement>) {}

    fn set_title(&self, _title: &str) {}

    fn set_editor_text(&self, _text: &str) {}
}

/// Run `work` with an extension UI whose dialogs show on `terminal` before the session exists.
/// Call inside a `LocalSet`.
pub async fn run_with_startup_ui<T, Fut: Future<Output = T>>(
    terminal: TerminalRef,
    settings: &SettingsManager,
    work: impl FnOnce(Arc<dyn ExtensionUi>) -> Fut,
) -> T {
    let (requests, mut receiver) = mpsc::unbounded_channel();
    let work = work(Arc::new(StartupExtensionUi { requests }));
    tokio::pin!(work);
    loop {
        tokio::select! {
            result = &mut work => return result,
            Some(request) = receiver.recv() => match request {
                StartupRequest::Select { title, options, reply } => {
                    let _ = reply.send(show_startup_selector(terminal.clone(), settings, &title, options).await);
                }
                StartupRequest::Input { title, reply } => {
                    let _ = reply.send(show_startup_input(terminal.clone(), settings, &title).await);
                }
            },
        }
    }
}

/// The project trust decision at interactive startup: `project_trust` handlers of extensions
/// first, with startup dialogs as their UI, then the trust prompt. Call inside a `LocalSet`.
pub async fn startup_project_trust(
    options: &crate::app::AppOptions,
    cwd: &Path,
) -> (Option<bool>, Vec<crate::agent_session::RuntimeDiagnostic>) {
    let terminal: TerminalRef = Rc::new(RefCell::new(ProcessTerminal::new()));
    let settings = SettingsManager::create(cwd, &options.agent_dir, SettingsOptions { project_trusted: false });
    let (decision, diagnostics) = run_with_startup_ui(terminal.clone(), &settings, |ui| {
        let mut options = options.clone();
        options.project_trust_ui = Some((ui, crate::extension_ui::ExtensionMode::Tui));
        async move { crate::app::extension_trust_decision(&options, cwd).await }
    })
    .await;
    let override_ = options.project_trust_override;
    let trusted = prompt_project_trust_with(terminal, cwd, &options.agent_dir, override_, decision).await;
    (trusted, diagnostics)
}
