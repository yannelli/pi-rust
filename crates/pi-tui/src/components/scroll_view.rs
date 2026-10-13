//! [`ScrollView`]: a vertical viewport over one child, driven by the fullscreen layout engine.

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::time::Duration;

use crate::component::{
    Component, ComponentRef, MouseLayout, MouseResponse, TuiMouseEvent, dispatch_children_mouse, invalidate_all,
};
use crate::layout_node::{LayoutNode, ScrollLayoutNode};
use crate::ui_loop::{self, TimerId};

/// Text styling function used for scrollbar glyphs and similar decorations.
pub type StyleFn = Rc<dyn Fn(&str) -> String>;

/// When the scrollbar is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrollViewScrollbar {
    #[default]
    Hidden,
    /// Shown briefly after scrolling or while hovered, when the content overflows.
    Auto,
    Always,
}

/// Whether the view sticks to the end of the content as it grows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrollFollow {
    #[default]
    None,
    End,
}

/// What happens to wheel scrolling past the edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrollOverscroll {
    /// Pass the rest to the enclosing scroll views.
    #[default]
    Chain,
    /// Stop at this view.
    Contain,
}

/// Options for [`ScrollView::new`].
#[derive(Clone, Default)]
pub struct ScrollViewOptions {
    pub follow: ScrollFollow,
    /// The primary view receives page keys and transcript search.
    pub primary: bool,
    pub overscroll: ScrollOverscroll,
    pub scrollbar: ScrollViewScrollbar,
    pub scrollbar_track_style: Option<StyleFn>,
    pub scrollbar_thumb_style: Option<StyleFn>,
    /// Delay before an `Auto` scrollbar hides again (default 1000 ms).
    pub scrollbar_hide_delay_ms: Option<u64>,
}

/// Options for [`ScrollViewHandle::scroll_to`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScrollViewScrollToOptions {
    /// Keep follow-end off even when the target is the current content end.
    pub disable_follow: bool,
}

pub struct ScrollViewState {
    follow_end: bool,
    primary: bool,
    overscroll: ScrollOverscroll,
    track_style: StyleFn,
    thumb_style: StyleFn,
    scrollbar: ScrollViewScrollbar,
    hide_delay: Duration,
    scroll_top: usize,
    content_height: usize,
    viewport_height: usize,
    following_end: bool,
    follow_suppressed_at_end: bool,
    request_render: Option<Rc<dyn Fn()>>,
    transient_visible: bool,
    active: bool,
    hide_timer: Option<TimerId>,
}

/// Shared scroll state of a [`ScrollView`]. Clones refer to the same view; compare with `ptr_eq`.
#[derive(Clone)]
pub struct ScrollViewHandle(Rc<RefCell<ScrollViewState>>);

impl ScrollViewHandle {
    fn new(options: &ScrollViewOptions) -> Self {
        let follow_end = options.follow == ScrollFollow::End;
        Self(Rc::new(RefCell::new(ScrollViewState {
            follow_end,
            primary: options.primary,
            overscroll: options.overscroll,
            track_style: options
                .scrollbar_track_style
                .clone()
                .unwrap_or_else(|| Rc::new(|t| format!("\x1b[90m{t}\x1b[39m"))),
            thumb_style: options
                .scrollbar_thumb_style
                .clone()
                .unwrap_or_else(|| Rc::new(|t| format!("\x1b[37m{t}\x1b[39m"))),
            scrollbar: options.scrollbar,
            hide_delay: Duration::from_millis(options.scrollbar_hide_delay_ms.unwrap_or(1000)),
            scroll_top: 0,
            content_height: 0,
            viewport_height: 0,
            following_end: follow_end,
            follow_suppressed_at_end: false,
            request_render: None,
            transient_visible: false,
            active: false,
            hide_timer: None,
        })))
    }

    pub fn ptr_eq(&self, other: &ScrollViewHandle) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }

    fn notify(&self) {
        let callback = self.0.borrow().request_render.clone();
        if let Some(callback) = callback {
            callback();
        }
    }

    pub fn scroll_top(&self) -> usize {
        self.0.borrow().scroll_top
    }

    pub fn is_following_end(&self) -> bool {
        self.0.borrow().following_end
    }

    pub fn viewport_height(&self) -> usize {
        self.0.borrow().viewport_height
    }

    pub fn follow_end(&self) -> bool {
        self.0.borrow().follow_end
    }

    pub fn primary(&self) -> bool {
        self.0.borrow().primary
    }

    pub fn overscroll(&self) -> ScrollOverscroll {
        self.0.borrow().overscroll
    }

    pub fn scrollbar(&self) -> ScrollViewScrollbar {
        self.0.borrow().scrollbar
    }

    pub fn is_scrollbar_visible(&self) -> bool {
        let state = self.0.borrow();
        match state.scrollbar {
            ScrollViewScrollbar::Always => state.viewport_height > 0,
            ScrollViewScrollbar::Auto => state.content_height > state.viewport_height && state.transient_visible,
            ScrollViewScrollbar::Hidden => false,
        }
    }

    pub fn is_scrollbar_active(&self) -> bool {
        self.0.borrow().active
    }

    pub fn scrollbar_track_style(&self, text: &str) -> String {
        let style = self.0.borrow().track_style.clone();
        style(text)
    }

    pub fn scrollbar_thumb_style(&self, text: &str) -> String {
        let style = self.0.borrow().thumb_style.clone();
        style(text)
    }

    pub fn set_scrollbar(&self, scrollbar: ScrollViewScrollbar) {
        {
            let state = self.0.borrow();
            if state.scrollbar == scrollbar {
                return;
            }
        }
        self.0.borrow_mut().scrollbar = scrollbar;
        if scrollbar != ScrollViewScrollbar::Auto {
            self.hide_transient_scrollbar();
        } else if self.0.borrow().active {
            self.mark_scrollbar_activity();
        }
        self.notify();
    }

    /// Content width for a viewport width: one column less when the scrollbar is always shown.
    pub fn get_content_width(&self, width: usize) -> usize {
        if self.0.borrow().scrollbar == ScrollViewScrollbar::Always && width > 1 { width - 1 } else { width }
    }

    fn mark_scrollbar_activity(&self) {
        let mut state = self.0.borrow_mut();
        if state.scrollbar != ScrollViewScrollbar::Auto || state.content_height <= state.viewport_height {
            return;
        }
        state.transient_visible = true;
        if let Some(timer) = state.hide_timer.take() {
            ui_loop::clear_timer(timer);
        }
        if state.active {
            return;
        }
        let weak: Weak<RefCell<ScrollViewState>> = Rc::downgrade(&self.0);
        state.hide_timer = Some(ui_loop::set_timeout(state.hide_delay, move || {
            let Some(state) = weak.upgrade() else { return };
            let callback = {
                let mut state = state.borrow_mut();
                state.hide_timer = None;
                state.transient_visible = false;
                state.request_render.clone()
            };
            if let Some(callback) = callback {
                callback();
            }
        }));
    }

    fn hide_transient_scrollbar(&self) {
        let mut state = self.0.borrow_mut();
        state.transient_visible = false;
        if let Some(timer) = state.hide_timer.take() {
            ui_loop::clear_timer(timer);
        }
    }

    pub fn set_scrollbar_active(&self, active: bool) {
        if self.0.borrow().active == active {
            return;
        }
        self.0.borrow_mut().active = active;
        self.mark_scrollbar_activity();
        self.notify();
    }

    fn max_scroll_top(&self) -> usize {
        let state = self.0.borrow();
        state.content_height.saturating_sub(state.viewport_height)
    }

    pub fn scroll_to(&self, scroll_top: i64, options: ScrollViewScrollToOptions) {
        let max = self.max_scroll_top();
        let next = scroll_top.clamp(0, max as i64) as usize;
        let suppressed = options.disable_follow && next == max;
        let following = {
            let state = self.0.borrow();
            !suppressed && state.follow_end && next == max
        };
        let moved = {
            let mut state = self.0.borrow_mut();
            if next == state.scroll_top
                && following == state.following_end
                && suppressed == state.follow_suppressed_at_end
            {
                return;
            }
            let moved = next != state.scroll_top;
            state.scroll_top = next;
            state.following_end = following;
            state.follow_suppressed_at_end = suppressed;
            moved
        };
        if moved {
            self.mark_scrollbar_activity();
        }
        self.notify();
    }

    /// Scrolls by `lines` and returns the part that could not be applied.
    pub fn scroll_by(&self, lines: i64) -> i64 {
        if lines == 0 {
            return 0;
        }
        let max = self.max_scroll_top() as i64;
        let (moved, changed_follow) = {
            let mut state = self.0.borrow_mut();
            let start = if state.following_end { max } else { state.scroll_top as i64 };
            let next = (start + lines).clamp(0, max);
            let was_following = state.following_end;
            state.scroll_top = next as usize;
            state.following_end = state.follow_end && next == max;
            state.follow_suppressed_at_end = false;
            (next - start, state.following_end != was_following)
        };
        if moved != 0 {
            self.mark_scrollbar_activity();
        }
        if moved != 0 || changed_follow {
            self.notify();
        }
        lines - moved
    }

    pub fn scroll_to_start(&self) {
        let changed = {
            let mut state = self.0.borrow_mut();
            let following = state.follow_end && state.content_height <= state.viewport_height;
            let changed = state.scroll_top != 0 || state.following_end != following;
            state.scroll_top = 0;
            state.following_end = following;
            state.follow_suppressed_at_end = false;
            changed
        };
        if changed {
            self.mark_scrollbar_activity();
            self.notify();
        }
    }

    pub fn scroll_to_end(&self) {
        let max = self.max_scroll_top();
        let changed = {
            let mut state = self.0.borrow_mut();
            let changed = state.scroll_top != max || state.following_end != state.follow_end;
            state.scroll_top = max;
            state.following_end = state.follow_end;
            state.follow_suppressed_at_end = false;
            changed
        };
        if changed {
            self.mark_scrollbar_activity();
            self.notify();
        }
    }

    /// Called by the layout engine with the measured content and viewport heights.
    pub fn update_layout(&self, content_height: usize, viewport_height: usize, request_render: Rc<dyn Fn()>) {
        let hide = {
            let mut state = self.0.borrow_mut();
            state.content_height = content_height;
            state.viewport_height = viewport_height;
            state.request_render = Some(request_render);
            let max = content_height.saturating_sub(viewport_height);
            state.scroll_top = if state.following_end { max } else { state.scroll_top.min(max) };
            if state.scroll_top < max {
                state.follow_suppressed_at_end = false;
            }
            if state.follow_end && state.scroll_top == max && !state.follow_suppressed_at_end {
                state.following_end = true;
            }
            content_height <= viewport_height
        };
        if hide {
            self.hide_transient_scrollbar();
        }
    }
}

/// A vertical scroll viewport over exactly one child.
pub struct ScrollView {
    child: ComponentRef,
    handle: ScrollViewHandle,
    mouse_layout: MouseLayout,
}

impl ScrollView {
    pub fn new(child: ComponentRef, options: ScrollViewOptions) -> Self {
        Self { child, handle: ScrollViewHandle::new(&options), mouse_layout: None }
    }

    /// Shared scroll state, also reachable while the view is borrowed by the renderer.
    pub fn handle(&self) -> ScrollViewHandle {
        self.handle.clone()
    }

    pub fn child(&self) -> ComponentRef {
        self.child.clone()
    }
}

impl std::ops::Deref for ScrollView {
    type Target = ScrollViewHandle;
    fn deref(&self) -> &ScrollViewHandle {
        &self.handle
    }
}

impl Component for ScrollView {
    fn render(&mut self, width: usize) -> Vec<String> {
        let content_width = self.handle.get_content_width(width);
        let lines = self.child.borrow_mut().render(content_width);
        let heights = vec![(self.child.clone(), lines.len())];
        self.mouse_layout = Some((width, heights));
        if content_width == width { lines } else { lines.into_iter().map(|line| format!("{line} ")).collect() }
    }

    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<MouseResponse> {
        let children = [self.child.clone()];
        dispatch_children_mouse(&children, self.mouse_layout.as_ref(), event).map(MouseResponse::Forwarded)
    }

    fn invalidate(&mut self) {
        invalidate_all(std::slice::from_ref(&self.child));
    }

    fn container_children(&self) -> Option<Vec<ComponentRef>> {
        Some(vec![self.child.clone()])
    }

    fn layout_node(&self) -> Option<LayoutNode> {
        Some(LayoutNode::Scroll(ScrollLayoutNode { component: self.child.clone(), state: self.handle.clone() }))
    }
}
