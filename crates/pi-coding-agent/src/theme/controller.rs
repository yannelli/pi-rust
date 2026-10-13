//! Applies the theme setting and keeps it in sync with the terminal (upstream `theme-controller.ts`).
//! The theme applies at once; the terminal's colors update it when they arrive, and the system theme
//! renders grayscale until then. Runs on the UI thread, like the TUI it drives.

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;

use pi_tui::Tui;
use pi_tui::terminal_colors::TerminalColors;
use tokio::sync::watch;

use super::ThemeError;
use super::registry::{init_theme, set_theme, set_theme_instance};
use super::system::SYSTEM_THEME_NAME;
use super::terminal::{
    TerminalTheme, get_terminal_theme, mark_terminal_colors_pending, parse_auto_theme_setting, resolve_theme_setting,
    set_terminal_color_scheme, set_terminal_colors,
};
use super::theme::Theme;

/// How long the system theme stays grayscale before falling back to palette indices. Replies that
/// arrive later still apply.
pub const TERMINAL_QUERY_TIMEOUT: Duration = Duration::from_millis(100);

const IN_MEMORY: &str = "<in-memory>";

/// What the controller needs from the TUI. Implemented for [`Tui`].
pub trait ThemeHost {
    /// Query the terminal colors; `on_result` runs on completion or timeout, `on_late_reply` after a timeout.
    fn query_terminal_colors(
        &self,
        timeout: Duration,
        on_result: Box<dyn FnOnce(TerminalColors)>,
        on_late_reply: Box<dyn FnOnce(TerminalColors)>,
    );
    fn set_terminal_color_scheme_notifications(&self, enabled: bool);
    /// Subscribe to light/dark reports; returns the unsubscribe function.
    fn on_terminal_color_scheme_change(&self, listener: Box<dyn FnMut(TerminalTheme)>) -> Box<dyn FnOnce()>;
    fn invalidate(&self);
    fn request_render(&self);
}

impl ThemeHost for Tui {
    fn query_terminal_colors(
        &self,
        timeout: Duration,
        on_result: Box<dyn FnOnce(TerminalColors)>,
        on_late_reply: Box<dyn FnOnce(TerminalColors)>,
    ) {
        Tui::query_terminal_colors(self, timeout, on_result, Some(on_late_reply));
    }

    fn set_terminal_color_scheme_notifications(&self, enabled: bool) {
        Tui::set_terminal_color_scheme_notifications(self, enabled);
    }

    fn on_terminal_color_scheme_change(&self, mut listener: Box<dyn FnMut(TerminalTheme)>) -> Box<dyn FnOnce()> {
        let id = Tui::on_terminal_color_scheme_change(self, move |scheme| listener(scheme.into()));
        let tui = self.downgrade();
        Box::new(move || {
            if let Some(tui) = tui.upgrade() {
                tui.remove_terminal_color_scheme_listener(id);
            }
        })
    }

    fn invalidate(&self) {
        Tui::invalidate(self);
    }

    fn request_render(&self) {
        Tui::request_render(self, false);
    }
}

/// Callbacks into the application.
pub struct ThemeControllerOptions {
    /// The saved `theme` setting (the settings manager's `theme_setting()`).
    pub get_theme_setting: Box<dyn Fn() -> Option<String>>,
    pub show_error: Box<dyn Fn(&str)>,
    pub on_changed: Box<dyn Fn()>,
    /// A `--use-theme` selection that overrides the saved setting without changing it.
    pub initial_theme_setting: Option<String>,
}

/// A theme for the extension API's `setTheme`: a name, or a theme built in memory.
#[derive(Clone)]
pub enum ThemeSelection {
    Name(String),
    Instance(Arc<Theme>),
}

struct State {
    host: Rc<dyn ThemeHost>,
    get_theme_setting: Rc<dyn Fn() -> Option<String>>,
    show_error: Rc<dyn Fn(&str)>,
    on_changed: Rc<dyn Fn()>,
    current_setting: Option<String>,
    // Last reported colors; a query that times out keeps them instead of erasing them.
    terminal_colors: Option<TerminalColors>,
    active_theme_name: Option<String>,
    auto_sync: bool,
    unsubscribe: Option<Box<dyn FnOnce()>>,
    queries_started: u64,
    settled: watch::Sender<u64>,
}

/// See the module docs. Cheap to clone; clones share state.
#[derive(Clone)]
pub struct InteractiveThemeController {
    state: Rc<RefCell<State>>,
}

fn merge(previous: Option<&TerminalColors>, reported: TerminalColors) -> TerminalColors {
    TerminalColors {
        foreground: reported.foreground.or(previous.and_then(|colors| colors.foreground)),
        background: reported.background.or(previous.and_then(|colors| colors.background)),
        palette: reported.palette.or_else(|| previous.and_then(|colors| colors.palette.clone())),
    }
}

impl InteractiveThemeController {
    pub fn new(host: Rc<dyn ThemeHost>, options: ThemeControllerOptions) -> Self {
        let state = State {
            host,
            get_theme_setting: Rc::from(options.get_theme_setting),
            show_error: Rc::from(options.show_error),
            on_changed: Rc::from(options.on_changed),
            current_setting: options.initial_theme_setting,
            terminal_colors: None,
            active_theme_name: None,
            auto_sync: false,
            unsubscribe: None,
            queries_started: 0,
            settled: watch::channel(0).0,
        };
        let controller = Self { state: Rc::new(RefCell::new(state)) };
        let name = controller.resolve_theme_name();
        controller.state.borrow_mut().active_theme_name = Some(name.clone());
        // The system theme starts grayscale; color follows once the terminal reports its colors.
        mark_terminal_colors_pending();
        init_theme(Some(&name), true);
        controller.bind_color_scheme_listener();
        controller
    }

    fn host(&self) -> Rc<dyn ThemeHost> {
        self.state.borrow().host.clone()
    }

    /// Subscribe again after the TUI was recreated.
    pub fn rebind_tui(&self, host: Rc<dyn ThemeHost>) {
        let unsubscribe = {
            let mut state = self.state.borrow_mut();
            state.host = host;
            state.unsubscribe.take()
        };
        if let Some(unsubscribe) = unsubscribe {
            unsubscribe();
        }
        self.bind_color_scheme_listener();
        let enabled = self.state.borrow().auto_sync;
        self.host().set_terminal_color_scheme_notifications(enabled);
    }

    /// Apply the setting now and query the terminal colors. Pairs and `system` follow appearance changes.
    pub fn apply_from_settings(&self) {
        let setting = self.theme_setting();
        let name = self.resolve_theme_name();
        self.set_auto_sync(parse_auto_theme_setting(setting.as_deref()).is_some() || name == SYSTEM_THEME_NAME);
        let _ = self.apply_theme_name(&name, setting.is_some());
        self.query_terminal_colors();
    }

    /// Resolves when the latest color query completed or timed out. Drive the UI loop while awaiting it.
    pub fn wait_for_terminal_colors(&self) -> impl Future<Output = ()> + use<> {
        let (target, mut receiver) = {
            let state = self.state.borrow();
            (state.queries_started, state.settled.subscribe())
        };
        async move {
            let _ = receiver.wait_for(|settled| *settled >= target).await;
        }
    }

    /// Whether the latest color query completed or timed out.
    pub fn terminal_colors_settled(&self) -> bool {
        let state = self.state.borrow();
        *state.settled.borrow() >= state.queries_started
    }

    pub fn get_theme_selection(&self) -> Option<String> {
        let (current, saved, active) = {
            let state = self.state.borrow();
            (state.current_setting.clone(), state.get_theme_setting.clone(), state.active_theme_name.clone())
        };
        current.or_else(|| saved()).or(active)
    }

    pub fn set_theme_name(&self, name: &str, show_error: bool) -> Result<(), ThemeError> {
        self.set_auto_sync(name == SYSTEM_THEME_NAME);
        let result = self.apply_theme_name(name, show_error);
        if result.is_ok() {
            self.state.borrow_mut().current_setting = Some(name.to_string());
        }
        result
    }

    pub fn set_theme_setting(&self, setting: &str) {
        self.state.borrow_mut().current_setting = Some(setting.to_string());
        self.apply_from_settings();
    }

    pub fn set_theme_instance(&self, theme: Arc<Theme>) -> Result<(), ThemeError> {
        self.set_auto_sync(false);
        set_theme_instance(theme);
        self.state.borrow_mut().active_theme_name = Some(IN_MEMORY.to_string());
        self.notify_changed();
        Ok(())
    }

    /// The extension API's `setTheme`. The caller persists a named selection in the settings.
    pub fn select(&self, selection: ThemeSelection) -> Result<(), ThemeError> {
        match selection {
            ThemeSelection::Name(name) => self.set_theme_name(&name, false),
            ThemeSelection::Instance(theme) => self.set_theme_instance(theme),
        }
    }

    /// Show a theme or setting without saving it (theme selector preview).
    pub fn preview(&self, setting_or_name: &str) {
        let active = self.state.borrow().active_theme_name.clone();
        let Some(name) = resolve_theme_setting(Some(setting_or_name), get_terminal_theme()).or(active) else { return };
        if set_theme(&name, true).is_ok() {
            let host = self.host();
            host.invalidate();
            host.request_render();
        }
    }

    pub fn disable_auto_sync(&self) {
        self.set_auto_sync(false);
    }

    pub fn dispose(&self) {
        self.set_auto_sync(false);
        let unsubscribe = self.state.borrow_mut().unsubscribe.take();
        if let Some(unsubscribe) = unsubscribe {
            unsubscribe();
        }
    }

    pub fn get_terminal_theme(&self) -> TerminalTheme {
        get_terminal_theme()
    }

    fn theme_setting(&self) -> Option<String> {
        let (current, saved) = {
            let state = self.state.borrow();
            (state.current_setting.clone(), state.get_theme_setting.clone())
        };
        current.or_else(|| saved())
    }

    /// The theme for the setting and terminal appearance; `system` without a setting.
    fn resolve_theme_name(&self) -> String {
        resolve_theme_setting(self.theme_setting().as_deref(), get_terminal_theme())
            .unwrap_or_else(|| SYSTEM_THEME_NAME.to_string())
    }

    fn apply_theme_name(&self, name: &str, show_error: bool) -> Result<(), ThemeError> {
        let result = set_theme(name, true);
        let active = if result.is_ok() { name } else { SYSTEM_THEME_NAME };
        self.state.borrow_mut().active_theme_name = Some(active.to_string());
        self.notify_changed();
        if let Err(error) = &result
            && show_error
        {
            let message = format!("Failed to load theme \"{name}\": {error}\nFell back to the system theme.");
            let show_error = self.state.borrow().show_error.clone();
            show_error(&message);
        }
        result
    }

    fn weak(&self) -> Weak<RefCell<State>> {
        Rc::downgrade(&self.state)
    }

    fn query_terminal_colors(&self) {
        let sequence = {
            let mut state = self.state.borrow_mut();
            state.queries_started += 1;
            state.queries_started
        };
        let (on_result, on_late) = (self.weak(), self.weak());
        self.host().query_terminal_colors(
            TERMINAL_QUERY_TIMEOUT,
            Box::new(move |colors| {
                if let Some(state) = on_result.upgrade() {
                    let controller = InteractiveThemeController { state };
                    controller.apply_terminal_colors(colors);
                    controller.state.borrow().settled.send_modify(|settled| *settled = (*settled).max(sequence));
                }
            }),
            Box::new(move |colors| {
                if let Some(state) = on_late.upgrade() {
                    InteractiveThemeController { state }.apply_terminal_colors(colors);
                }
            }),
        );
    }

    /// Record reported colors and re-render, unless nothing changed (a timeout keeps the known colors).
    fn apply_terminal_colors(&self, reported: TerminalColors) {
        {
            let mut state = self.state.borrow_mut();
            let next = merge(state.terminal_colors.as_ref(), reported);
            if state.terminal_colors.as_ref() == Some(&next) {
                return;
            }
            state.terminal_colors = Some(next.clone());
            set_terminal_colors(next);
        }
        self.reapply_for_terminal();
        let host = self.host();
        host.invalidate();
        host.request_render();
    }

    /// Regenerate the system theme or switch the theme of a pair; extension and preview themes stay.
    fn reapply_for_terminal(&self) {
        let active = self.state.borrow().active_theme_name.clone();
        if active.as_deref() == Some(IN_MEMORY) {
            return;
        }
        let name = self.resolve_theme_name();
        if name == SYSTEM_THEME_NAME || Some(&name) != active.as_ref() {
            let _ = self.apply_theme_name(&name, false);
        }
    }

    fn set_auto_sync(&self, enabled: bool) {
        {
            let mut state = self.state.borrow_mut();
            if state.auto_sync == enabled {
                return;
            }
            state.auto_sync = enabled;
        }
        self.host().set_terminal_color_scheme_notifications(enabled);
    }

    fn bind_color_scheme_listener(&self) {
        let weak = self.weak();
        let unsubscribe = self.host().on_terminal_color_scheme_change(Box::new(move |scheme| {
            if let Some(state) = weak.upgrade() {
                InteractiveThemeController { state }.apply_color_scheme_change(scheme);
            }
        }));
        self.state.borrow_mut().unsubscribe = Some(unsubscribe);
    }

    /// The terminal reported a light/dark switch. Its colors changed too, so query them again; the
    /// reported scheme only matters for terminals that do not report their background.
    fn apply_color_scheme_change(&self, scheme: TerminalTheme) {
        if !self.state.borrow().auto_sync {
            return;
        }
        let previous = get_terminal_theme();
        set_terminal_color_scheme(Some(scheme));
        if get_terminal_theme() != previous {
            self.reapply_for_terminal();
        }
        self.query_terminal_colors();
    }

    fn notify_changed(&self) {
        self.host().invalidate();
        let on_changed = self.state.borrow().on_changed.clone();
        on_changed();
    }
}
