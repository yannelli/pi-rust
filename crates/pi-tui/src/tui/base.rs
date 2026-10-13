//! Shared TUI state, render scheduling, and start/stop (upstream `TuiBase`).

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::input::PendingColorQuery;
use super::overlay::{OverlayEntry, RenderedOverlayLayout};
use super::{Screen, TuiError, TuiInputListenerResult, TuiStopOptions};
use crate::component::{ComponentRef, MouseLayout, TuiMouseDispatchResult, TuiMouseEvent};
use crate::env::EnvLookup;
use crate::terminal::TerminalRef;
use crate::terminal_colors::TerminalColorScheme;
use crate::terminal_image::get_capabilities;
use crate::ui_loop::{self, LoopClient, TerminalSink};

const MIN_RENDER_INTERVAL: Duration = Duration::from_millis(16);

/// Identifier of a registered listener.
pub type ListenerId = u64;

pub(crate) type InputListener = Rc<RefCell<dyn FnMut(&str) -> TuiInputListenerResult>>;
pub(crate) type DebugCallback = Rc<RefCell<Box<dyn FnMut()>>>;
pub(crate) type SchemeListener = Rc<RefCell<dyn FnMut(TerminalColorScheme)>>;

static NEXT_TUI_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static TUIS: RefCell<Vec<Weak<TuiInner>>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn register_tui(inner: &Rc<TuiInner>) {
    TUIS.with(|tuis| {
        let mut tuis = tuis.borrow_mut();
        tuis.retain(|t| t.strong_count() > 0);
        tuis.push(Rc::downgrade(inner));
    });
}

/// Finds a live TUI of this thread by id.
pub(crate) fn find_tui(id: u64) -> Option<Rc<TuiInner>> {
    TUIS.with(|tuis| tuis.borrow().iter().filter_map(Weak::upgrade).find(|t| t.id == id))
}

pub(crate) fn next_tui_id() -> u64 {
    NEXT_TUI_ID.fetch_add(1, Ordering::Relaxed)
}

/// What to resume when a blocked overlay focus restore ends.
#[derive(Clone)]
pub(crate) enum BlockedResume {
    RestoreOverlay,
    FocusTarget(Option<ComponentRef>),
}

/// Whether focus should return to an overlay once the component that took it lets go.
#[derive(Clone)]
pub(crate) enum FocusRestore {
    Inactive,
    Eligible { overlay: Rc<OverlayEntry> },
    Blocked { overlay: Rc<OverlayEntry>, blocked_by: ComponentRef, resume: BlockedResume },
}

impl FocusRestore {
    pub fn overlay(&self) -> Option<&Rc<OverlayEntry>> {
        match self {
            FocusRestore::Inactive => None,
            FocusRestore::Eligible { overlay } | FocusRestore::Blocked { overlay, .. } => Some(overlay),
        }
    }
}

pub(crate) struct BaseState {
    pub focused: Option<ComponentRef>,
    pub input_listeners: Vec<(ListenerId, InputListener)>,
    pub render_requested: bool,
    pub immediate_render: bool,
    pub render_deadline: Option<Instant>,
    pub last_render_at: Option<Instant>,
    pub show_hardware_cursor: bool,
    pub clear_on_shrink: bool,
    pub full_redraw_count: usize,
    pub stopped: bool,
    pub started: bool,
    pub pending_color_queries: VecDeque<Rc<RefCell<PendingColorQuery>>>,
    pub color_scheme_listeners: Vec<(ListenerId, SchemeListener)>,
    pub color_scheme_notifications: bool,
    pub focus_order_counter: u64,
    pub overlay_stack: Vec<Rc<OverlayEntry>>,
    pub rendered_overlay_layouts: Vec<RenderedOverlayLayout>,
    pub overlay_focus_restore: FocusRestore,
    listener_counter: ListenerId,
}

impl BaseState {
    pub fn new(show_hardware_cursor: bool) -> Self {
        Self {
            focused: None,
            input_listeners: Vec::new(),
            render_requested: false,
            immediate_render: false,
            render_deadline: None,
            last_render_at: None,
            show_hardware_cursor,
            clear_on_shrink: false,
            full_redraw_count: 0,
            stopped: false,
            started: false,
            pending_color_queries: VecDeque::new(),
            color_scheme_listeners: Vec::new(),
            color_scheme_notifications: false,
            focus_order_counter: 0,
            overlay_stack: Vec::new(),
            rendered_overlay_layouts: Vec::new(),
            overlay_focus_restore: FocusRestore::Inactive,
            listener_counter: 0,
        }
    }

    pub fn next_listener_id(&mut self) -> ListenerId {
        self.listener_counter += 1;
        self.listener_counter
    }
}

pub(crate) struct TuiInner {
    pub id: u64,
    pub weak: Weak<TuiInner>,
    pub terminal: TerminalRef,
    pub env: Rc<dyn EnvLookup>,
    pub log_directory: Option<PathBuf>,
    pub state: RefCell<BaseState>,
    pub screen: RefCell<Screen>,
    pub children: RefCell<Vec<ComponentRef>>,
    /// Fullscreen layout root; when set it replaces the root children.
    pub layout_root: RefCell<Option<ComponentRef>>,
    pub mouse_layout: RefCell<MouseLayout>,
    pub on_debug: RefCell<Option<DebugCallback>>,
    /// Focus flags that could not be set because the component was borrowed at the time.
    pub pending_focus_flags: RefCell<Vec<(ComponentRef, bool)>>,
    pub dispatch_depth: Cell<u32>,
    /// The TUI that took over the terminal; render requests of this stopped TUI go there.
    pub successor: RefCell<Option<Weak<TuiInner>>>,
}

impl TuiInner {
    pub fn is_alt(&self) -> bool {
        matches!(&*self.screen.borrow(), Screen::Alt(_))
    }

    pub fn is_stopped(&self) -> bool {
        self.state.borrow().stopped
    }

    pub fn columns(&self) -> usize {
        self.terminal.borrow().columns()
    }

    pub fn rows(&self) -> usize {
        self.terminal.borrow().rows()
    }

    pub fn write(&self, data: &str) {
        self.terminal.borrow_mut().write(data);
    }

    /// Hides the cursor while running. After `stop`, the shell owns the cursor.
    pub fn hide_terminal_cursor(&self) {
        if !self.is_stopped() {
            self.terminal.borrow_mut().hide_cursor();
        }
    }

    pub fn set_show_hardware_cursor(&self, enabled: bool) {
        {
            let mut state = self.state.borrow_mut();
            if state.show_hardware_cursor == enabled {
                return;
            }
            state.show_hardware_cursor = enabled;
        }
        if !enabled {
            self.hide_terminal_cursor();
        }
        self.request_render(false);
    }

    /// Sets a component's focus flag, or queues it while the component is borrowed.
    pub fn set_focus_flag(&self, component: &ComponentRef, focused: bool) {
        match component.try_borrow_mut() {
            Ok(mut c) => c.set_focused(focused),
            Err(_) => self.pending_focus_flags.borrow_mut().push((component.clone(), focused)),
        }
    }

    pub fn apply_pending_focus_flags(&self) {
        let pending = std::mem::take(&mut *self.pending_focus_flags.borrow_mut());
        for (component, focused) in pending {
            self.set_focus_flag(&component, focused);
        }
    }

    /// Roots that count as mounted for focus restoration (fullscreen: the layout root if set).
    pub fn mounted_roots(&self) -> Vec<ComponentRef> {
        if let Some(root) = self.layout_root.borrow().clone() {
            return vec![root];
        }
        self.children.borrow().clone()
    }

    /// Base container render of the root children.
    pub fn render_children(&self, width: usize) -> Vec<String> {
        let children = self.children.borrow().clone();
        let mut layout = None;
        let lines = crate::component::render_children_with_layout(&children, width, &mut layout);
        *self.mouse_layout.borrow_mut() = layout;
        lines
    }

    /// Upstream `render(width)`: the fullscreen layout root, or the root children.
    pub fn render_root(&self, width: usize) -> Vec<String> {
        let layout_root = self.layout_root.borrow().clone();
        match layout_root {
            Some(root) => root.borrow_mut().render(width),
            None => self.render_children(width),
        }
    }

    pub fn dispatch_root_mouse(&self, event: &TuiMouseEvent) -> Option<TuiMouseDispatchResult> {
        let children = self.children.borrow().clone();
        let layout = self.mouse_layout.borrow().clone();
        crate::component::dispatch_children_mouse(&children, layout.as_ref(), event)
    }

    pub fn invalidate_all(&self) {
        crate::component::invalidate_all(&self.mounted_roots());
        let overlays: Vec<ComponentRef> =
            self.state.borrow().overlay_stack.iter().map(|o| o.component.clone()).collect();
        crate::component::invalidate_all(&overlays);
    }

    pub fn start(&self) {
        self.state.borrow_mut().stopped = false;
        self.state.borrow_mut().started = true;
        self.before_terminal_start();
        let sink = TerminalSink::new(ui_loop::waker(), self.id);
        self.terminal.borrow_mut().start(sink);
        self.terminal.borrow_mut().hide_cursor();
        if self.state.borrow().color_scheme_notifications {
            self.write("\x1b[?2031h");
        }
        if get_capabilities().images.is_some() {
            // Cell size in pixels: CSI 16 t, answered with CSI 6 ; height ; width t.
            self.write("\x1b[16t");
        }
        self.request_render(false);
    }

    pub fn stop(&self, options: TuiStopOptions) {
        let notifications = {
            let mut state = self.state.borrow_mut();
            state.stopped = true;
            state.render_deadline = None;
            state.render_requested = false;
            state.immediate_render = false;
            state.color_scheme_notifications
        };
        if notifications {
            self.write("\x1b[?2031l");
        }
        self.before_terminal_stop(options);
        self.terminal.borrow_mut().show_cursor();
        self.terminal.borrow_mut().stop();
        self.after_terminal_stop(options);
    }

    fn before_terminal_start(&self) {
        if self.is_alt() {
            crate::alt_screen::before_terminal_start(self);
        }
    }

    fn before_terminal_stop(&self, options: TuiStopOptions) {
        let main = matches!(&*self.screen.borrow(), Screen::Main(_));
        if main {
            crate::main_screen::before_terminal_stop(self, options);
        } else {
            crate::alt_screen::before_terminal_stop(self, options);
        }
    }

    fn after_terminal_stop(&self, options: TuiStopOptions) {
        if self.is_alt() {
            crate::alt_screen::after_terminal_stop(self, options);
        }
    }

    pub fn reset_render_state(&self) {
        match &mut *self.screen.borrow_mut() {
            Screen::Main(main) => main.reset(),
            Screen::Alt(alt) => alt.reset_render_state(),
        }
    }

    fn do_render(&self) -> Result<(), TuiError> {
        self.apply_pending_focus_flags();
        if self.is_alt() { crate::alt_screen::do_render(self) } else { crate::main_screen::do_render(self) }
    }

    pub fn render_now(&self, force: bool) -> Result<(), TuiError> {
        if force {
            self.reset_render_state();
        }
        {
            let mut state = self.state.borrow_mut();
            state.render_requested = false;
            state.immediate_render = false;
            state.render_deadline = None;
            state.last_render_at = Some(Instant::now());
        }
        self.do_render()
    }

    pub fn request_render(&self, force: bool) {
        if force {
            self.reset_render_state();
            self.request_immediate_render();
            return;
        }
        let mut state = self.state.borrow_mut();
        if state.render_requested {
            return;
        }
        state.render_requested = true;
        let earliest = state.last_render_at.map(|at| at + MIN_RENDER_INTERVAL);
        state.render_deadline = Some(earliest.map_or_else(Instant::now, |at| at.max(Instant::now())));
    }

    /// Keyboard input renders on the next loop pass instead of waiting for the throttle.
    pub fn request_immediate_render(&self) {
        let mut state = self.state.borrow_mut();
        state.render_requested = true;
        state.immediate_render = true;
        state.render_deadline = None;
    }

    pub fn add_input_listener(&self, listener: InputListener) -> ListenerId {
        let mut state = self.state.borrow_mut();
        let id = state.next_listener_id();
        state.input_listeners.push((id, listener));
        id
    }

    pub fn set_color_scheme_notifications(&self, enabled: bool) {
        let stopped = {
            let mut state = self.state.borrow_mut();
            if state.color_scheme_notifications == enabled {
                return;
            }
            state.color_scheme_notifications = enabled;
            state.stopped || !state.started
        };
        if !stopped {
            self.write(if enabled { "\x1b[?2031h" } else { "\x1b[?2031l" });
        }
    }
}

impl LoopClient for TuiInner {
    fn id(&self) -> u64 {
        self.id
    }

    fn handle_terminal_input(&self, data: String) {
        TuiInner::handle_terminal_input(self, data);
    }

    fn handle_resize(&self) {
        self.request_render(false);
    }

    fn request_render_from_loop(&self, force: bool) {
        if self.state.borrow().started {
            self.request_render(force);
        }
    }

    fn render_deadline(&self) -> Option<Instant> {
        let state = self.state.borrow();
        if state.stopped || !state.render_requested {
            return None;
        }
        if state.immediate_render {
            return Some(Instant::now());
        }
        state.render_deadline
    }

    fn flush_render(&self, now: Instant) -> Result<(), TuiError> {
        let due = {
            let state = self.state.borrow();
            !state.stopped
                && state.render_requested
                && (state.immediate_render || state.render_deadline.is_some_and(|d| d <= now))
        };
        if !due {
            return Ok(());
        }
        {
            let mut state = self.state.borrow_mut();
            state.render_requested = false;
            state.immediate_render = false;
            state.render_deadline = None;
            state.last_render_at = Some(Instant::now());
        }
        self.do_render()?;
        // A render requested during this render waits for the throttle interval.
        let mut state = self.state.borrow_mut();
        if state.render_requested && !state.immediate_render {
            state.render_deadline = state.last_render_at.map(|at| at + MIN_RENDER_INTERVAL);
        }
        Ok(())
    }
}
