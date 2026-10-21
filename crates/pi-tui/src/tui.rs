//! The TUI: focus, overlays, input routing, render scheduling, and terminal queries.
//! [`Tui`] is a cheap-clone handle; components may keep one to request renders or move focus.

mod base;
mod composite;
mod focus;
mod input;
mod overlay;

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::time::Duration;

pub use composite::{composite_tui_line, extract_cursor_position, normalize_line_resets, resolve_fake_cursors};
pub use overlay::{
    OverlayAnchor, OverlayBounds, OverlayHandle, OverlayMargin, OverlayOptions, OverlayUnfocusOptions, SizeValue,
};

use base::{BaseState, ListenerId};
pub(crate) use base::{TuiInner, find_tui};

use crate::alt_screen::{AltScreenState, TuiAltScreenOptions};
use crate::component::{ComponentRef, TuiMouseEvent};
use crate::env::{EnvLookup, ProcessEnv};
use crate::main_screen::{MainScreenState, TuiMainScreenRenderState};
use crate::terminal::TerminalRef;
use crate::terminal_colors::{TerminalColorScheme, TerminalColors};
use crate::ui_loop;

/// Rendering mode of a TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiMode {
    /// Renders into the main screen and its scrollback.
    Regular,
    /// Renders into the alternate screen with an application-owned viewport.
    Fullscreen,
}

/// Options for [`Tui::stop`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TuiStopOptions {
    /// Leave renderer output in place for another TUI taking over the same terminal.
    pub preserve_screen: bool,
}

/// Construction options shared by both modes.
#[derive(Default, Clone)]
pub struct TuiOptions {
    /// Show the hardware cursor at the focused component's cursor marker.
    pub show_hardware_cursor: Option<bool>,
    /// Directory for debug and crash logs. Without it, debug logging is off and crash dumps go to
    /// the temp directory.
    pub log_directory: Option<PathBuf>,
    /// Environment lookup (`PI_TUI_DEBUG`, `PI_TUI_DEBUG_REDRAW`, `TERM`, `TMUX`, ...). Defaults to
    /// the process environment.
    pub env: Option<Rc<dyn EnvLookup>>,
}

/// Errors from rendering.
#[derive(Debug)]
pub enum TuiError {
    /// A rendered line was wider than the terminal. The TUI was stopped and a crash log written.
    LineTooWide { line: usize, width: usize, terminal_width: usize, crash_log: PathBuf },
}

impl std::fmt::Display for TuiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TuiError::LineTooWide { line, width, terminal_width, crash_log } => write!(
                f,
                "Rendered line {line} exceeds terminal width ({width} > {terminal_width}).\n\n\
                 This is likely caused by a custom TUI component not truncating its output.\n\
                 Use visibleWidth() to measure and truncateToWidth() to truncate lines.\n\n\
                 Debug log written to: {}",
                crash_log.display()
            ),
        }
    }
}

impl std::error::Error for TuiError {}

/// What an input listener did with the data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiInputListenerResult {
    /// Pass the data on unchanged.
    Pass,
    /// Stop processing this input.
    Consume,
    /// Replace the data for later listeners and the focused component. Empty data stops processing.
    Replace(String),
}

/// Handle returned by input and color scheme listener registration.
pub type TuiListenerId = ListenerId;

/// Cheap-clone handle to a TUI. Single-threaded; see the crate docs for the threading model.
#[derive(Clone)]
pub struct Tui {
    pub(crate) inner: Rc<TuiInner>,
}

impl Tui {
    /// TUI that renders into the main screen and scrollback (upstream `TuiMainScreen`).
    pub fn new_main_screen(terminal: TerminalRef, options: TuiOptions) -> Tui {
        Self::create(terminal, options, |_| Screen::Main(MainScreenState::default()))
    }

    /// Fullscreen TUI on the alternate screen (upstream `TuiAltScreen`).
    pub fn new_alt_screen(terminal: TerminalRef, options: TuiOptions, alt: TuiAltScreenOptions) -> Tui {
        Self::create(terminal, options, move |weak| Screen::Alt(Box::new(AltScreenState::new(weak, alt))))
    }

    fn create(terminal: TerminalRef, options: TuiOptions, screen: impl FnOnce(Weak<TuiInner>) -> Screen) -> Tui {
        let env: Rc<dyn EnvLookup> = options.env.unwrap_or_else(|| Rc::new(ProcessEnv));
        let inner = Rc::new_cyclic(|weak: &Weak<TuiInner>| TuiInner {
            id: base::next_tui_id(),
            weak: weak.clone(),
            terminal,
            env,
            log_directory: options.log_directory,
            state: RefCell::new(BaseState::new(options.show_hardware_cursor.unwrap_or(false))),
            screen: RefCell::new(screen(weak.clone())),
            children: RefCell::new(Vec::new()),
            layout_root: RefCell::new(None),
            mouse_layout: RefCell::new(None),
            on_debug: RefCell::new(None),
            pending_focus_flags: RefCell::new(Vec::new()),
            dispatch_depth: Cell::new(0),
            successor: RefCell::new(None),
        });
        let client: Rc<dyn ui_loop::LoopClient> = inner.clone();
        ui_loop::register_client(Rc::downgrade(&client));
        base::register_tui(&inner);
        Tui { inner }
    }

    pub(crate) fn from_inner(inner: Rc<TuiInner>) -> Tui {
        Tui { inner }
    }

    /// A weak handle that does not keep the TUI alive.
    pub fn downgrade(&self) -> WeakTui {
        WeakTui(Rc::downgrade(&self.inner))
    }

    pub fn mode(&self) -> TuiMode {
        if self.inner.is_alt() { TuiMode::Fullscreen } else { TuiMode::Regular }
    }

    /// Whether this TUI owns a scrollable viewport (upstream `isViewportTUI`).
    pub fn is_viewport(&self) -> bool {
        self.inner.is_alt()
    }

    pub fn terminal(&self) -> TerminalRef {
        self.inner.terminal.clone()
    }

    /// Number of full redraws so far.
    pub fn full_redraws(&self) -> usize {
        self.inner.state.borrow().full_redraw_count
    }

    pub fn add_child(&self, component: ComponentRef) {
        self.inner.children.borrow_mut().push(component);
    }

    pub fn remove_child(&self, component: &ComponentRef) {
        let mut children = self.inner.children.borrow_mut();
        if let Some(index) = children.iter().position(|c| crate::component::same_component(c, component)) {
            children.remove(index);
        }
    }

    pub fn clear(&self) {
        self.inner.children.borrow_mut().clear();
    }

    /// Root children, in render order.
    pub fn children(&self) -> Vec<ComponentRef> {
        self.inner.children.borrow().clone()
    }

    /// Callback for the global debug key (Shift+Ctrl+D), run before input reaches the focused component.
    pub fn set_on_debug(&self, callback: Option<Box<dyn FnMut()>>) {
        *self.inner.on_debug.borrow_mut() = callback.map(|c| Rc::new(RefCell::new(c)) as _);
    }

    pub fn get_show_hardware_cursor(&self) -> bool {
        self.inner.state.borrow().show_hardware_cursor
    }

    pub fn set_show_hardware_cursor(&self, enabled: bool) {
        self.inner.set_show_hardware_cursor(enabled);
    }

    pub fn get_clear_on_shrink(&self) -> bool {
        self.inner.state.borrow().clear_on_shrink
    }

    /// When true, a shrink of the content triggers a full redraw that clears the empty rows.
    pub fn set_clear_on_shrink(&self, enabled: bool) {
        self.inner.state.borrow_mut().clear_on_shrink = enabled;
    }

    pub fn get_focused_component(&self) -> Option<ComponentRef> {
        self.inner.state.borrow().focused.clone()
    }

    pub fn set_focus(&self, component: Option<ComponentRef>) {
        self.inner.set_focus(component);
    }

    /// Shows an overlay on top of the content and returns a handle to control it.
    pub fn show_overlay(&self, component: ComponentRef, options: OverlayOptions) -> OverlayHandle {
        self.inner.show_overlay(component, options)
    }

    /// Hides the topmost overlay and restores focus.
    pub fn hide_overlay(&self) {
        self.inner.hide_top_overlay();
    }

    /// Whether any overlay is visible.
    pub fn has_overlay(&self) -> bool {
        self.inner.has_visible_overlay()
    }

    /// Invalidates every mounted component and overlay.
    pub fn invalidate(&self) {
        self.inner.invalidate_all();
    }

    pub fn start(&self) {
        self.inner.start();
    }

    pub fn stop(&self, options: TuiStopOptions) {
        self.inner.stop(options);
    }

    /// Renders at once. `force` drops the last frame so everything is drawn again.
    pub fn render_now(&self, force: bool) -> Result<(), TuiError> {
        self.inner.render_now(force)
    }

    /// Schedules a render (throttled to one per 16 ms). `force` redraws everything right away.
    pub fn request_render(&self, force: bool) {
        let successor = self.inner.successor.borrow().as_ref().and_then(Weak::upgrade);
        match successor {
            Some(next) => Tui::from_inner(next).request_render(force),
            None => self.inner.request_render(force),
        }
    }

    /// Forwards render requests to `next` once this TUI is replaced, so components created for
    /// this TUI keep repainting after a switch between regular and fullscreen mode.
    pub fn hand_over_to(&self, next: &Tui) {
        *self.inner.successor.borrow_mut() = Some(Rc::downgrade(&next.inner));
    }

    /// Adds a listener that sees input before the focused component.
    pub fn add_input_listener(&self, listener: impl FnMut(&str) -> TuiInputListenerResult + 'static) -> TuiListenerId {
        self.inner.add_input_listener(Rc::new(RefCell::new(listener)))
    }

    pub fn remove_input_listener(&self, id: TuiListenerId) {
        self.inner.state.borrow_mut().input_listeners.retain(|(lid, _)| *lid != id);
    }

    /// Calls `listener` when the terminal reports a color scheme change (CSI ? 997 ; n n).
    pub fn on_terminal_color_scheme_change(
        &self,
        listener: impl FnMut(TerminalColorScheme) + 'static,
    ) -> TuiListenerId {
        let mut state = self.inner.state.borrow_mut();
        let id = state.next_listener_id();
        state.color_scheme_listeners.push((id, Rc::new(RefCell::new(listener))));
        id
    }

    pub fn remove_terminal_color_scheme_listener(&self, id: TuiListenerId) {
        self.inner.state.borrow_mut().color_scheme_listeners.retain(|(lid, _)| *lid != id);
    }

    /// Enables or disables color scheme change notifications (mode 2031).
    pub fn set_terminal_color_scheme_notifications(&self, enabled: bool) {
        self.inner.set_color_scheme_notifications(enabled);
    }

    /// Queries the default colors and palette. `on_result` runs on the DA1 reply, when every color
    /// replied, or at the timeout; `on_late_reply` receives replies that complete after the timeout.
    pub fn query_terminal_colors(
        &self,
        timeout: Duration,
        on_result: impl FnOnce(TerminalColors) + 'static,
        on_late_reply: Option<Box<dyn FnOnce(TerminalColors)>>,
    ) {
        self.inner.query_terminal_colors(timeout, Box::new(on_result), on_late_reply);
    }

    /// Async form of [`Tui::query_terminal_colors`]. Drive the UI loop while awaiting it.
    pub fn query_terminal_colors_async(
        &self,
        timeout: Duration,
        on_late_reply: Option<Box<dyn FnOnce(TerminalColors)>>,
    ) -> impl std::future::Future<Output = TerminalColors> + use<> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.query_terminal_colors(
            timeout,
            move |colors| {
                let _ = tx.send(colors);
            },
            on_late_reply,
        );
        async move { rx.await.unwrap_or_default() }
    }

    /// Feeds one input sequence as if the terminal delivered it.
    pub fn handle_terminal_input(&self, data: &str) {
        self.inner.handle_terminal_input(data.to_string());
    }

    /// Renders the root children (upstream `TUI.render`; in fullscreen mode, the layout root).
    pub fn render(&self, width: usize) -> Vec<String> {
        self.inner.render_root(width)
    }

    /// Dispatches a mouse event to the root children (upstream `Container.handleMouse`).
    pub fn handle_mouse(&self, event: &TuiMouseEvent) -> Option<crate::component::TuiMouseDispatchResult> {
        self.inner.dispatch_root_mouse(event)
    }

    /// Main screen: snapshot of the renderer state for a TUI taking over the terminal.
    pub fn capture_render_state(&self) -> Option<TuiMainScreenRenderState> {
        match &*self.inner.screen.borrow() {
            Screen::Main(main) => Some(main.capture()),
            Screen::Alt(_) => None,
        }
    }

    /// Main screen: continues from a snapshot taken by another TUI.
    pub fn restore_render_state(&self, state: TuiMainScreenRenderState) {
        if let Screen::Main(main) = &mut *self.inner.screen.borrow_mut() {
            main.restore(state);
        }
    }
}

/// Restores the terminal modes of every TUI of this thread that was started and not stopped: mouse
/// tracking, focus reporting, bracketed paste, the Kitty keyboard protocol, and the alternate
/// screen. A dropped TUI does this on its own; call this before `std::process::exit`, which runs no
/// destructors. Hard kills and aborts cannot be covered.
pub fn restore_terminals_on_exit() {
    base::restore_all_on_exit();
}

/// Weak counterpart of [`Tui`].
#[derive(Clone)]
pub struct WeakTui(Weak<TuiInner>);

impl WeakTui {
    pub fn upgrade(&self) -> Option<Tui> {
        self.0.upgrade().map(Tui::from_inner)
    }
}

pub(crate) enum Screen {
    Main(MainScreenState),
    Alt(Box<AltScreenState>),
}
