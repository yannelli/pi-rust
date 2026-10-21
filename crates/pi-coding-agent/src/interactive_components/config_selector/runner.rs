//! Shows the config selector on the process terminal (upstream `cli/config-selector.ts`). The
//! UI is not `Send`, so the future starts a UI thread that owns the terminal and its loop, and
//! awaits a oneshot that the thread completes when the selector closes.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use futures::future::BoxFuture;
use pi_tui::{
    ComponentRef, ProcessTerminal, TerminalRef, Tui, TuiOptions, TuiStopOptions, restore_terminals_on_exit, ui_loop,
};

use super::ConfigSelector;
use crate::config::agent_dir;
use crate::packages::{ConfigSelectorFn, ConfigSelectorRequest};
use crate::theme::{init_theme, stop_theme_watcher};

/// A [`ConfigSelectorFn`] for `pi config` on a terminal: each call runs
/// [`run_config_selector`] on a new UI thread and resolves when the selector closes.
pub fn config_selector_fn() -> ConfigSelectorFn {
    Arc::new(|request: ConfigSelectorRequest| -> BoxFuture<'static, ()> {
        Box::pin(async move {
            let (done, closed) = tokio::sync::oneshot::channel::<()>();
            let thread = std::thread::Builder::new().name("pi-config-ui".into()).spawn(move || {
                run_config_selector(request);
                let _ = done.send(());
            });
            if thread.is_ok() {
                let _ = closed.await;
            }
        })
    })
}

/// Shows the selector in the main screen of the process terminal and blocks this thread until
/// Esc closes it. Ctrl+C, when not bound to cancel, stops the TUI and exits the process.
pub fn run_config_selector(request: ConfigSelectorRequest) {
    let ConfigSelectorRequest { editor, project_mode_available } = request;
    let settings = editor.settings().clone();
    init_theme(settings.theme().as_deref(), true);

    let terminal: TerminalRef = Rc::new(RefCell::new(ProcessTerminal::new()));
    let options = TuiOptions {
        show_hardware_cursor: Some(settings.show_hardware_cursor()),
        log_directory: Some(agent_dir()),
        env: None,
    };
    let tui = Tui::new_main_screen(terminal.clone(), options);
    tui.set_clear_on_shrink(settings.clear_on_shrink());
    let rows = terminal.borrow().rows();

    let closed = Rc::new(Cell::new(false));
    let on_close = {
        let closed = closed.clone();
        move || closed.set(true)
    };
    let exit_tui = tui.downgrade();
    let on_exit = move || {
        if let Some(tui) = exit_tui.upgrade() {
            tui.stop(TuiStopOptions::default());
        }
        stop_theme_watcher();
        restore_terminals_on_exit();
        std::process::exit(0);
    };
    let render_tui = tui.downgrade();
    let request_render = move || {
        if let Some(tui) = render_tui.upgrade() {
            tui.request_render(false);
        }
    };
    let selector = ConfigSelector::new(editor, on_close, on_exit, request_render, Some(rows), project_mode_available);
    let selector: ComponentRef = Rc::new(RefCell::new(selector));
    tui.add_child(selector.clone());
    tui.set_focus(Some(selector));
    tui.start();
    while !closed.get() {
        if ui_loop::run_once(None).is_err() {
            break;
        }
    }
    tui.stop(TuiStopOptions::default());
    stop_theme_watcher();
}
