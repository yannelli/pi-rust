//! `runClientTui` of upstream `client-tui.ts`: open the client runtime and run the TUI until the user exits.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use pi_chord::facets::FacetLoader;
use pi_chord::{ChordError, ChordResult};
use pi_tui::{ProcessTerminal, TerminalRef, Tui, TuiAltScreenOptions, TuiOptions, ui_loop};

use super::cli::ClientCommand;
use super::client_runtime::{ClientRuntime, OpenClientRuntimeOptions, open_client_runtime};
use super::client_tui::{ClientTuiOptions, ExperimentalClientTui};
use super::client_tui_parts::ClientTuiServer;
use crate::settings::{SettingsManager, SettingsOptions};
use crate::theme::{InteractiveThemeController, ThemeControllerOptions};

/// Upstream `RunClientTuiOptions`.
#[derive(Default)]
pub struct RunClientTuiOptions {
    pub runtime: OpenClientRuntimeOptions,
    pub facet_loader: Option<Arc<dyn FacetLoader>>,
}

/// Run the experimental client TUI on the terminal.
pub async fn run_client_tui(command: &ClientCommand, options: RunClientTuiOptions) -> ChordResult<()> {
    let cwd = std::env::current_dir().map_err(ChordError::other)?;
    let agent_dir = crate::config::agent_dir();
    let settings = Arc::new(SettingsManager::create(&cwd, &agent_dir, SettingsOptions::default()));
    // The system theme until the controller resolves the user's theme against the terminal's colors.
    crate::theme::init_theme(None, false);
    crate::keybindings::KeybindingsManager::create(&agent_dir).install();
    let runtime = open_client_runtime(command, options.runtime).await?;
    let local = tokio::task::LocalSet::new();
    let ran = local.run_until(run(command, &runtime, options.facet_loader, settings, agent_dir)).await;
    let disposed = runtime.dispose().await;
    ran.and(disposed)
}

async fn run(
    command: &ClientCommand,
    runtime: &ClientRuntime,
    facet_loader: Option<Arc<dyn FacetLoader>>,
    settings: Arc<SettingsManager>,
    agent_dir: std::path::PathBuf,
) -> ChordResult<()> {
    let terminal: TerminalRef = Rc::new(RefCell::new(ProcessTerminal::new()));
    let options = TuiOptions { log_directory: Some(agent_dir), ..TuiOptions::default() };
    let tui = Tui::new_alt_screen(terminal, options, TuiAltScreenOptions::default());
    tui.set_show_hardware_cursor(settings.show_hardware_cursor());
    tui.set_clear_on_shrink(settings.clear_on_shrink());
    let finished = Rc::new(Cell::new(false));
    let done = finished.clone();
    let finish: Rc<dyn Fn()> = Rc::new(move || {
        done.set(true);
        ui_loop::waker().wake();
    });
    let render = tui.clone();
    let component = ExperimentalClientTui::create(ClientTuiOptions {
        command: command.clone(),
        tui: tui.clone(),
        servers: runtime.servers.iter().map(ClientTuiServer::from_runtime).collect(),
        facet_loader,
        request_render: Rc::new(move || render.request_render(false)),
        finish,
    })
    .await?;
    for child in component.children() {
        tui.add_child(child);
    }
    tui.set_layout_root(Some(component.layout_root()));
    tui.set_focus(Some(component.input_component()));
    let (theme_settings, changed) = (settings.clone(), Rc::downgrade(&component));
    let themes = InteractiveThemeController::new(
        Rc::new(tui.clone()),
        ThemeControllerOptions {
            get_theme_setting: Box::new(move || theme_settings.theme_setting()),
            show_error: Box::new(|message| eprintln!("{message}")),
            on_changed: Box::new(move || {
                if let Some(component) = changed.upgrade() {
                    component.refresh_theme();
                }
            }),
            initial_theme_setting: None,
        },
    );
    tui.start();
    themes.apply_from_settings();
    let mut result = Ok(());
    while !finished.get() {
        if let Err(error) = ui_loop::run_once_async().await {
            result = Err(ChordError::error(error.to_string()));
            break;
        }
    }
    themes.dispose();
    tui.stop(pi_tui::TuiStopOptions::default());
    let closed = component.close().await;
    result.and(closed)
}
