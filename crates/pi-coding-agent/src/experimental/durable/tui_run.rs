//! `runDurableTui` of upstream `durable/tui.ts`, and `main.ts` of `durable/` and `vacation/`.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use pi_durable::harness::types::{ModelRef, WhenBusy};
use pi_durable::{ConversationId, DurableError, DurableResult};
use pi_tui::components::select_list::SelectItem;
use pi_tui::terminal_image::{CapabilityOverrides, ImageProtocol, set_capability_overrides};
use pi_tui::{ProcessTerminal, TerminalRef, Tui, TuiAltScreenOptions, TuiOptions, component, ui_loop};

use super::controller::DurableController;
use super::open::{DurableApp, open_durable, parse_durable_args};
use super::runtime::{ViewStore, agent_of, conversation_id_of};
use super::tui::DurableTui;
use super::tui_parts::ListSelector;
use crate::settings::SettingsManager;
use crate::theme::{InteractiveThemeController, ThemeControllerOptions};

/// Run `f` once the current input dispatch returns, when no component is borrowed.
fn defer(f: impl FnOnce() + 'static) {
    ui_loop::set_timeout(Duration::ZERO, f);
    ui_loop::waker().wake();
}

/// The `terminal.images`, `terminal.trueColor`, and `terminal.hyperlinks` settings.
fn apply_capability_overrides(settings: &SettingsManager) {
    let (images, true_color, hyperlinks) = settings.terminal_capability_overrides();
    let images = images.map(|kind| match kind.as_deref() {
        Some("kitty") => Some(ImageProtocol::Kitty),
        Some("iterm2") => Some(ImageProtocol::Iterm2),
        _ => None,
    });
    set_capability_overrides(CapabilityOverrides { images, true_color, hyperlinks });
}

fn select_model(view: &Rc<DurableTui>, source: &Arc<ViewStore>, controller: &DurableController) {
    let snapshot = source.current();
    let current = agent_of(&snapshot.conversation).model;
    let is_current = |provider: &str, id: &str| {
        current.as_ref().is_some_and(|model| model.provider == provider && model.model_id == id)
    };
    let mut models = snapshot.models.clone();
    models.sort_by_key(|model| !is_current(&model.provider, &model.model_id));
    let items = models
        .iter()
        .map(|model| {
            SelectItem::new(&format!("{}/{}", model.provider, model.model_id), &model.model_id)
                .with_description(&model.provider)
        })
        .collect();
    let (selected, cancelled, controller) = (view.clone(), view.clone(), controller.clone());
    let selector = ListSelector::new(
        "Select model:",
        items,
        Rc::new(move |value: String| {
            let (view, controller) = (selected.clone(), controller.clone());
            defer(move || {
                view.restore_editor();
                if let Some((provider, model_id)) = value.split_once('/') {
                    let reference = ModelRef { provider: provider.into(), model_id: model_id.into() };
                    tokio::spawn(controller.set_model(reference));
                }
            });
        }),
        Rc::new(move || {
            let view = cancelled.clone();
            defer(move || view.restore_editor());
        }),
    );
    view.mount(component(selector));
}

fn select_conversation(view: &Rc<DurableTui>, source: &Arc<ViewStore>, controller: &DurableController) {
    let snapshot = source.current();
    let shown = conversation_id_of(&snapshot.conversation);
    let items = snapshot
        .conversations
        .iter()
        .rev()
        .map(|candidate| {
            let marker = if Some(candidate.id) == shown { "(shown) " } else { "" };
            let description = format!("{marker}{}", candidate.title.clone().unwrap_or_default());
            SelectItem::new(&candidate.id.to_string(), &candidate.label).with_description(&description)
        })
        .collect();
    let (selected, cancelled, controller) = (view.clone(), view.clone(), controller.clone());
    let selector = ListSelector::new(
        "Switch to:",
        items,
        Rc::new(move |value: String| {
            let (view, controller) = (selected.clone(), controller.clone());
            defer(move || {
                view.restore_editor();
                if let Ok(id) = serde_json::from_str::<ConversationId>(&value) {
                    tokio::spawn(controller.switch_conversation(id));
                }
            });
        }),
        Rc::new(move || {
            let view = cancelled.clone();
            defer(move || view.restore_editor());
        }),
    );
    view.mount(component(selector));
}

/// Render the view until the user exits. Must run inside a `LocalSet`.
pub async fn run_durable_tui(
    source: Arc<ViewStore>,
    controller: DurableController,
    settings: Arc<SettingsManager>,
    agent_dir: PathBuf,
) -> DurableResult<()> {
    apply_capability_overrides(&settings);
    // The system theme until the controller resolves the user's theme against the terminal's colors.
    crate::theme::init_theme(None, false);
    crate::keybindings::KeybindingsManager::create(&agent_dir).install();
    let terminal: TerminalRef = Rc::new(RefCell::new(ProcessTerminal::new()));
    let options = TuiOptions { log_directory: Some(agent_dir), ..TuiOptions::default() };
    let tui = Tui::new_alt_screen(terminal, options, TuiAltScreenOptions::default());
    let view = DurableTui::new(tui.clone(), PathBuf::from(&source.current().session.cwd));
    let exited = Rc::new(Cell::new(false));

    let editor = view.editor.clone();
    let (submit_view, submit_source, submit_controller) = (view.clone(), source.clone(), controller.clone());
    editor.on_submit(move |text: &str| {
        let trimmed = text.trim().to_string();
        let (view, source, controller) = (submit_view.clone(), submit_source.clone(), submit_controller.clone());
        defer(move || match trimmed.as_str() {
            "" => {}
            "/model" => select_model(&view, &source, &controller),
            "/tasks" => drop(tokio::spawn(controller.toggle_tasks())),
            "/agents" => select_conversation(&view, &source, &controller),
            command if command == "/compact" || command.starts_with("/compact ") => {
                let instructions = command["/compact".len()..].trim();
                let instructions = (!instructions.is_empty()).then(|| instructions.to_string());
                tokio::spawn(controller.compact(instructions));
            }
            _ => drop(tokio::spawn(controller.submit(trimmed, WhenBusy::Steer))),
        });
    });
    let abort = controller.clone();
    editor.set_on_escape(Some(Rc::new(move || drop(tokio::spawn(abort.abort())))));
    let exit = exited.clone();
    let exit_now: Rc<dyn Fn()> = Rc::new(move || {
        exit.set(true);
        ui_loop::waker().wake();
    });
    editor.set_on_ctrl_d(Some(exit_now.clone()));
    editor.on_action("app.clear", move || exit_now());
    let (model_view, model_source, model_controller) = (view.clone(), source.clone(), controller.clone());
    editor.on_action("app.model.select", move || {
        let (view, source, controller) = (model_view.clone(), model_source.clone(), model_controller.clone());
        defer(move || select_model(&view, &source, &controller));
    });
    let thinking = controller.clone();
    editor.on_action("app.thinking.cycle", move || drop(tokio::spawn(thinking.cycle_thinking())));
    let expand = view.clone();
    editor.on_action("app.tools.expand", move || {
        let view = expand.clone();
        defer(move || view.toggle_expanded());
    });
    let (follow_editor, follow_controller) = (editor.clone(), controller.clone());
    editor.on_action("app.message.followUp", move || {
        let text = follow_editor.get_text().trim().to_string();
        if text.is_empty() {
            return;
        }
        follow_editor.set_text("");
        tokio::spawn(follow_controller.submit(text, WhenBusy::FollowUp));
    });

    // pi's theme handling: the theme setting resolved against the terminal's reported colors.
    let (theme_settings, changed) = (settings.clone(), tui.clone());
    let themes = InteractiveThemeController::new(
        Rc::new(tui.clone()),
        ThemeControllerOptions {
            get_theme_setting: Box::new(move || theme_settings.theme_setting()),
            show_error: Box::new(|message| eprintln!("{message}")),
            on_changed: Box::new(move || changed.request_render(false)),
            initial_theme_setting: None,
        },
    );
    let mut changes = source.subscribe();
    let (apply_view, apply_source) = (view.clone(), source.clone());
    let follow = tokio::task::spawn_local(async move {
        while changes.changed().await.is_ok() {
            let (view, source) = (apply_view.clone(), apply_source.clone());
            defer(move || view.apply(&source.current()));
        }
    });
    tui.start();
    themes.apply_from_settings();
    view.apply(&source.current());
    let mut result = Ok(());
    while !exited.get() {
        if let Err(error) = ui_loop::run_once_async().await {
            result = Err(DurableError::Error(error.to_string()));
            break;
        }
    }
    follow.abort();
    themes.dispose();
    view.stop();
    result
}

/// Upstream `main.ts` of `durable/` (`DurableApp::Coding`) and `vacation/` (`DurableApp::Vacation`).
pub async fn run_durable_main(app: DurableApp, args: &[String]) -> DurableResult<()> {
    let options = parse_durable_args(args)?;
    let agent_dir = options.agent_dir.clone().unwrap_or_else(crate::config::agent_dir);
    let durable = open_durable(app, options).await?;
    let (view, controller, settings) = (durable.view.clone(), durable.controller.clone(), durable.settings.clone());
    let local = tokio::task::LocalSet::new();
    let ran = local.run_until(run_durable_tui(view, controller, settings, agent_dir)).await;
    let closed = durable.close().await;
    ran.and(closed)
}
