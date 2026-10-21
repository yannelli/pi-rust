//! Fullscreen renderer on the alternate screen: application-owned viewport, scrollbars, wheel,
//! text selection with copy-on-select, transcript search, and flashes (upstream `TuiAltScreen`).

mod compose;
mod images;
mod lifecycle;
mod mouse;
mod public;
mod scrollbar;
mod selection;
mod viewport;

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::time::Instant;

pub(crate) use lifecycle::{
    after_terminal_stop, before_terminal_start, before_terminal_stop, do_render, exit_restore_prefix,
    exit_restore_suffix,
};
pub use public::{CopyCompletion, CopyOutcome, CopyRequest, CopySelectionFn};
pub(crate) use viewport::handle_viewport_input;

use crate::alt_screen_search::{
    AltScreenSearchBoxState, AltScreenSearchIndex, AltScreenSearchMatch, NavigationButtonStyle,
};
use crate::component::{Component, ComponentRef, MouseResponse, TuiMouseDispatchTarget, TuiMouseEvent, invalidate_all};
use crate::components::alt_screen_flash::AltScreenFlashContainer;
use crate::components::scroll_view::{ScrollFollow, ScrollView, ScrollViewHandle, ScrollViewOptions, StyleFn};
use crate::layout::LayoutFrame;
use crate::terminal_image::{ImageProtocol, TerminalCapabilities};
use crate::tui::{OverlayHandle, Screen, TuiInner};
use crate::wheel_scroll::{WheelScrollAccelerator, WheelScrollLines};

/// Opens an OSC 8 link URL.
pub type UrlOpener = Rc<dyn Fn(&str)>;

/// Options for [`crate::Tui::new_alt_screen`].
#[derive(Clone, Default)]
pub struct TuiAltScreenOptions {
    /// Lines per wheel event (default 1). `Auto` accelerates fast spins; Alt+wheel moves five times as far.
    pub wheel_scroll_lines: Option<WheelScrollLines>,
    /// Capture mouse events for scrolling and application-owned selection (default true).
    pub mouse: Option<bool>,
    /// Style of a search match that is not the current one.
    pub search_match_style: Option<StyleFn>,
    /// Style of the current search match.
    pub search_current_match_style: Option<StyleFn>,
    /// Style of a search navigation button.
    pub search_navigation_button_style: Option<NavigationButtonStyle>,
    /// Label of the clickable jump-to-end indicator shown while a follow-end primary view is
    /// scrolled away from its end.
    pub scroll_to_end_indicator: Option<Rc<dyn Fn() -> String>>,
    /// Opens an OSC 8 link activated with a primary-button click.
    pub open_url: Option<UrlOpener>,
    /// Handles an unmodified right-button press for clipboard paste (Windows only).
    pub on_right_click_paste: Option<Rc<dyn Fn()>>,
    /// Copy the selection to the clipboard on mouse release (default true).
    pub copy_on_select: Option<bool>,
    /// Copies selected text to the system clipboard. Without it, the selection is copied with OSC 52.
    pub copy_selection: Option<CopySelectionFn>,
}

#[derive(Clone, Copy)]
pub(crate) struct CachedKittyImage {
    transmission_generation: u64,
    transmission_bytes: usize,
    estimated_decoded_bytes: usize,
}

#[derive(Clone)]
pub(crate) struct SelectionPoint {
    pub row: i64,
    pub col: i64,
    pub scroll_view: Option<ScrollViewHandle>,
    /// The point lies between cells rather than on a cell.
    pub boundary: bool,
}

pub(crate) fn same_view(a: &Option<ScrollViewHandle>, b: &Option<ScrollViewHandle>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.ptr_eq(b),
        (None, None) => true,
        _ => false,
    }
}

#[derive(Clone)]
pub(crate) struct SelectionRange {
    pub start: SelectionPoint,
    pub end: SelectionPoint,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelectionGranularity {
    Character,
    Word,
    Line,
}

pub(crate) struct ClickTarget {
    timestamp: Instant,
    count: u32,
    row: i64,
    scroll_view: Option<ScrollViewHandle>,
    word_start: i64,
    word_end: i64,
}

pub(crate) struct ComponentClick {
    timestamp: Instant,
    count: u32,
    component: ComponentRef,
    x: i32,
    y: i32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchSelectionMode {
    Query,
    Retain,
    Next,
    Previous,
}

pub(crate) struct ActiveSearch {
    pub box_state: Rc<RefCell<AltScreenSearchBoxState>>,
    pub index: AltScreenSearchIndex,
    pub overlay: Option<OverlayHandle>,
    pub query: String,
    pub matches: Vec<AltScreenSearchMatch>,
    pub selected_index: Option<usize>,
    pub selected_key: Option<String>,
    pub anchor_row: usize,
    pub selection_mode: SearchSelectionMode,
}

pub(crate) struct AltScreenState {
    pub previous_screen: Vec<String>,
    pub last_document: Vec<String>,
    pub previous_screen_width: usize,
    pub previous_screen_height: usize,
    pub current_layout: Option<LayoutFrame>,
    pub implicit_scroll_view: ComponentRef,
    pub implicit_scroll: ScrollViewHandle,
    pub flashes: AltScreenFlashContainer,
    pub alt_screen_active: bool,
    pub image_protocol: Option<ImageProtocol>,
    pub saved_capabilities: Option<TerminalCapabilities>,
    pub uploaded_kitty_images: Vec<(u32, CachedKittyImage)>,
    pub selection_anchor: Option<SelectionPoint>,
    pub selection_focus: Option<SelectionPoint>,
    pub selection_granularity: SelectionGranularity,
    pub selection_initial_range: Option<SelectionRange>,
    pub last_click: Option<ClickTarget>,
    pub selection_drag_pointer: Option<(i64, i64)>,
    pub selection_auto_scroll_direction: i64,
    pub selection_auto_scroll_timer: Option<crate::ui_loop::TimerId>,
    pub selection_press_active: bool,
    pub scrollbar_drag: Option<(ScrollViewHandle, i64)>,
    pub scrollbar_hover: Option<ScrollViewHandle>,
    pub scroll_to_end_indicator_rect: Option<(i64, i64, i64)>,
    pub active_search: Option<ActiveSearch>,
    pub pressed_url: Option<String>,
    pub selection_dragged: bool,
    pub mouse_capture: Option<TuiMouseDispatchTarget>,
    pub mouse_press_target: Option<TuiMouseDispatchTarget>,
    pub mouse_press_point: Option<(i32, i32)>,
    pub mouse_press_moved: bool,
    pub last_component_click: Option<ComponentClick>,
    pub wheel_scroll: WheelScrollAccelerator,
    pub mouse_enabled: bool,
    pub search_match_style: StyleFn,
    pub search_current_match_style: StyleFn,
    pub search_navigation_button_style: Option<NavigationButtonStyle>,
    pub scroll_to_end_indicator: Option<Rc<dyn Fn() -> String>>,
    pub open_url: Option<UrlOpener>,
    pub on_right_click_paste: Option<Rc<dyn Fn()>>,
    pub copy_on_select: bool,
    pub copy_selection: Option<CopySelectionFn>,
}

/// The root children of the TUI as one document, scrolled by the implicit scroll view.
struct TuiDocument {
    tui: Weak<TuiInner>,
}

impl Component for TuiDocument {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.tui.upgrade().map(|tui| tui.render_children(width)).unwrap_or_default()
    }

    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<MouseResponse> {
        self.tui.upgrade()?.dispatch_root_mouse(event).map(MouseResponse::Forwarded)
    }

    fn invalidate(&mut self) {
        if let Some(tui) = self.tui.upgrade() {
            invalidate_all(&tui.children.borrow().clone());
        }
    }

    fn container_children(&self) -> Option<Vec<ComponentRef>> {
        self.tui.upgrade().map(|tui| tui.children.borrow().clone())
    }
}

impl AltScreenState {
    pub fn new(weak: Weak<TuiInner>, options: TuiAltScreenOptions) -> Self {
        let document: ComponentRef = Rc::new(RefCell::new(TuiDocument { tui: weak.clone() }));
        let scroll_view = ScrollView::new(
            document,
            ScrollViewOptions { follow: ScrollFollow::End, primary: true, ..ScrollViewOptions::default() },
        );
        let implicit_scroll = scroll_view.handle();
        let render_weak = weak.clone();
        let request_render: Rc<dyn Fn()> = Rc::new(move || {
            if let Some(tui) = render_weak.upgrade() {
                tui.request_render(false);
            }
        });
        Self {
            previous_screen: Vec::new(),
            last_document: Vec::new(),
            previous_screen_width: 0,
            previous_screen_height: 0,
            current_layout: None,
            implicit_scroll_view: Rc::new(RefCell::new(scroll_view)),
            implicit_scroll,
            flashes: AltScreenFlashContainer::new(request_render),
            alt_screen_active: false,
            image_protocol: None,
            saved_capabilities: None,
            uploaded_kitty_images: Vec::new(),
            selection_anchor: None,
            selection_focus: None,
            selection_granularity: SelectionGranularity::Character,
            selection_initial_range: None,
            last_click: None,
            selection_drag_pointer: None,
            selection_auto_scroll_direction: 0,
            selection_auto_scroll_timer: None,
            selection_press_active: false,
            scrollbar_drag: None,
            scrollbar_hover: None,
            scroll_to_end_indicator_rect: None,
            active_search: None,
            pressed_url: None,
            selection_dragged: false,
            mouse_capture: None,
            mouse_press_target: None,
            mouse_press_point: None,
            mouse_press_moved: false,
            last_component_click: None,
            wheel_scroll: WheelScrollAccelerator::new(
                options.wheel_scroll_lines.unwrap_or(WheelScrollLines::Fixed(1.0)),
            ),
            mouse_enabled: options.mouse.unwrap_or(true),
            search_match_style: options
                .search_match_style
                .unwrap_or_else(|| Rc::new(|text| format!("\x1b[4m{text}\x1b[24m"))),
            search_current_match_style: options
                .search_current_match_style
                .unwrap_or_else(|| Rc::new(|text| format!("\x1b[1;7m{text}\x1b[22;27m"))),
            search_navigation_button_style: options.search_navigation_button_style,
            scroll_to_end_indicator: options.scroll_to_end_indicator,
            open_url: options.open_url,
            on_right_click_paste: options.on_right_click_paste,
            copy_on_select: options.copy_on_select.unwrap_or(true),
            copy_selection: options.copy_selection,
        }
    }

    pub fn reset_render_state(&mut self) {
        self.previous_screen.clear();
        self.previous_screen_width = 0;
        self.previous_screen_height = 0;
        self.current_layout = None;
    }

    pub fn primary_scroll_view(&self) -> ScrollViewHandle {
        self.current_layout
            .as_ref()
            .and_then(|layout| layout.primary_scroll_view.clone())
            .unwrap_or_else(|| self.implicit_scroll.clone())
    }

    pub fn clear_component_mouse_gesture(&mut self) {
        self.mouse_capture = None;
        self.mouse_press_target = None;
        self.mouse_press_point = None;
        self.mouse_press_moved = false;
    }
}

/// Runs `f` with the fullscreen state. Never call TUI or component methods inside `f`.
pub(crate) fn with_alt<R>(inner: &TuiInner, f: impl FnOnce(&mut AltScreenState) -> R) -> Option<R> {
    match &mut *inner.screen.borrow_mut() {
        Screen::Alt(alt) => Some(f(alt)),
        Screen::Main(_) => None,
    }
}

/// Milliseconds since an arbitrary process-wide start, like `performance.now()`.
pub(crate) fn now_ms() -> f64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64() * 1000.0
}
