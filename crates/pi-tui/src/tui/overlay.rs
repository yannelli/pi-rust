//! Overlay options, positioning, compositing, and the [`OverlayHandle`].

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use super::base::TuiInner;
use super::composite::{composite_tui_line, resolve_fake_cursors};
use crate::component::ComponentRef;
use crate::utils::{slice_by_column, visible_width};

/// Anchor position for overlays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverlayAnchor {
    #[default]
    Center,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    TopCenter,
    BottomCenter,
    LeftCenter,
    RightCenter,
}

/// Margin from the terminal edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OverlayMargin {
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub left: i32,
}

impl OverlayMargin {
    /// The same margin on every side.
    pub fn all(value: i32) -> Self {
        Self { top: value, right: value, bottom: value, left: value }
    }
}

/// A size or position that is absolute or a percentage of the terminal size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SizeValue {
    Absolute(i32),
    Percent(f64),
}

impl SizeValue {
    /// Parses `"50%"` or a plain number. Other strings return `None`.
    pub fn parse(value: &str) -> Option<SizeValue> {
        let value = value.trim();
        if let Some(percent) = value.strip_suffix('%') {
            let valid = !percent.is_empty() && percent.chars().all(|c| c.is_ascii_digit() || c == '.');
            return valid.then(|| percent.parse().ok().map(SizeValue::Percent)).flatten();
        }
        value.parse().ok().map(SizeValue::Absolute)
    }

    fn resolve(self, reference: usize) -> i64 {
        match self {
            SizeValue::Absolute(value) => i64::from(value),
            SizeValue::Percent(percent) => ((reference as f64 * percent) / 100.0).floor() as i64,
        }
    }
}

/// Visibility predicate called with the terminal `(columns, rows)` on each render.
pub type OverlayVisibility = Rc<dyn Fn(usize, usize) -> bool>;

/// Overlay positioning and sizing.
#[derive(Clone, Default)]
pub struct OverlayOptions {
    /// Width in columns or percent of the terminal width. Default: `min(80, available width)`.
    pub width: Option<SizeValue>,
    pub min_width: Option<i32>,
    /// Maximum height in rows or percent of the terminal height.
    pub max_height: Option<SizeValue>,
    /// Anchor point (default: center).
    pub anchor: Option<OverlayAnchor>,
    pub offset_x: Option<i32>,
    pub offset_y: Option<i32>,
    /// Row: absolute, or percent where 0% is the top and 100% the bottom.
    pub row: Option<SizeValue>,
    /// Column: absolute, or percent where 0% is the left and 100% the right.
    pub col: Option<SizeValue>,
    pub margin: Option<OverlayMargin>,
    /// The overlay renders only while this returns true.
    pub visible: Option<OverlayVisibility>,
    /// Do not take keyboard focus when shown.
    pub non_capturing: bool,
}

/// Options for [`OverlayHandle::unfocus`].
#[derive(Clone)]
pub struct OverlayUnfocusOptions {
    /// Explicit target to focus after releasing this overlay.
    pub target: Option<ComponentRef>,
}

/// Last rendered overlay rectangle, relative to the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayBounds {
    pub row: usize,
    pub col: usize,
    pub width: usize,
    pub height: usize,
}

pub(crate) struct OverlayEntry {
    pub component: ComponentRef,
    pub options: OverlayOptions,
    pub pre_focus: RefCell<Option<ComponentRef>>,
    pub hidden: Cell<bool>,
    pub focus_order: Cell<u64>,
    pub bounds: Cell<Option<OverlayBounds>>,
}

#[derive(Clone)]
pub(crate) struct RenderedOverlayLayout {
    pub entry: Rc<OverlayEntry>,
    pub row: usize,
    pub col: usize,
    pub width: usize,
    pub height: usize,
}

pub(crate) struct ResolvedOverlayLayout {
    pub width: usize,
    pub row: usize,
    pub col: usize,
    pub max_height: Option<usize>,
}

fn anchor_row(anchor: OverlayAnchor, height: i64, avail_height: i64, margin_top: i64) -> i64 {
    match anchor {
        OverlayAnchor::TopLeft | OverlayAnchor::TopCenter | OverlayAnchor::TopRight => margin_top,
        OverlayAnchor::BottomLeft | OverlayAnchor::BottomCenter | OverlayAnchor::BottomRight => {
            margin_top + avail_height - height
        }
        OverlayAnchor::LeftCenter | OverlayAnchor::Center | OverlayAnchor::RightCenter => {
            margin_top + (avail_height - height).div_euclid(2)
        }
    }
}

fn anchor_col(anchor: OverlayAnchor, width: i64, avail_width: i64, margin_left: i64) -> i64 {
    match anchor {
        OverlayAnchor::TopLeft | OverlayAnchor::LeftCenter | OverlayAnchor::BottomLeft => margin_left,
        OverlayAnchor::TopRight | OverlayAnchor::RightCenter | OverlayAnchor::BottomRight => {
            margin_left + avail_width - width
        }
        OverlayAnchor::TopCenter | OverlayAnchor::Center | OverlayAnchor::BottomCenter => {
            margin_left + (avail_width - width).div_euclid(2)
        }
    }
}

/// Resolves width, position, and max height of an overlay (upstream `resolveOverlayLayout`).
pub(crate) fn resolve_overlay_layout(
    options: &OverlayOptions,
    overlay_height: usize,
    term_width: usize,
    term_height: usize,
) -> ResolvedOverlayLayout {
    let margin = options.margin.unwrap_or_default();
    let (mt, mr, mb, ml) = (
        i64::from(margin.top.max(0)),
        i64::from(margin.right.max(0)),
        i64::from(margin.bottom.max(0)),
        i64::from(margin.left.max(0)),
    );
    let (tw, th) = (term_width as i64, term_height as i64);
    let avail_width = (tw - ml - mr).max(1);
    let avail_height = (th - mt - mb).max(1);

    let mut width = options.width.map(|w| w.resolve(term_width)).unwrap_or_else(|| 80.min(avail_width));
    if let Some(min_width) = options.min_width {
        width = width.max(i64::from(min_width));
    }
    width = width.min(avail_width).max(1);

    let max_height = options.max_height.map(|h| h.resolve(term_height).min(avail_height).max(1));
    let effective_height = match max_height {
        Some(max) => (overlay_height as i64).min(max),
        None => overlay_height as i64,
    };
    let anchor = options.anchor.unwrap_or_default();

    let mut row = match options.row {
        Some(SizeValue::Percent(percent)) => {
            let max_row = (avail_height - effective_height).max(0);
            mt + (max_row as f64 * percent / 100.0).floor() as i64
        }
        Some(SizeValue::Absolute(value)) => i64::from(value),
        None => anchor_row(anchor, effective_height, avail_height, mt),
    };
    let mut col = match options.col {
        Some(SizeValue::Percent(percent)) => {
            let max_col = (avail_width - width).max(0);
            ml + (max_col as f64 * percent / 100.0).floor() as i64
        }
        Some(SizeValue::Absolute(value)) => i64::from(value),
        None => anchor_col(anchor, width, avail_width, ml),
    };
    row += i64::from(options.offset_y.unwrap_or(0));
    col += i64::from(options.offset_x.unwrap_or(0));
    row = row.min(th - mb - effective_height).max(mt);
    col = col.min(tw - mr - width).max(ml);

    ResolvedOverlayLayout {
        width: width.max(1) as usize,
        row: row.max(0) as usize,
        col: col.max(0) as usize,
        max_height: max_height.map(|h| h.max(1) as usize),
    }
}

impl TuiInner {
    pub(crate) fn is_overlay_visible(&self, entry: &OverlayEntry) -> bool {
        if entry.hidden.get() {
            return false;
        }
        match &entry.options.visible {
            Some(visible) => {
                let (columns, rows) = (self.columns(), self.rows());
                visible(columns, rows)
            }
            None => true,
        }
    }

    /// Composites visible overlays into `lines`, ordered by focus order (higher is on top).
    pub(crate) fn composite_overlays(&self, lines: Vec<String>, term_width: usize, term_height: usize) -> Vec<String> {
        let stack: Vec<Rc<OverlayEntry>> = self.state.borrow().overlay_stack.clone();
        if stack.is_empty() {
            self.state.borrow_mut().rendered_overlay_layouts.clear();
            return lines;
        }
        let mut result = lines;
        for entry in &stack {
            entry.bounds.set(None);
        }
        let mut visible: Vec<Rc<OverlayEntry>> = stack.into_iter().filter(|e| self.is_overlay_visible(e)).collect();
        visible.sort_by_key(|e| e.focus_order.get());
        let show_hardware_cursor = self.state.borrow().show_hardware_cursor;
        let mut rendered = Vec::new();
        let mut min_lines_needed = result.len();
        for entry in visible {
            let layout = resolve_overlay_layout(&entry.options, 0, term_width, term_height);
            let lines = entry.component.borrow_mut().render(layout.width);
            let mut overlay_lines = resolve_fake_cursors(lines, show_hardware_cursor);
            if let Some(max_height) = layout.max_height {
                overlay_lines.truncate(max_height);
            }
            let placed = resolve_overlay_layout(&entry.options, overlay_lines.len(), term_width, term_height);
            entry.bounds.set(Some(OverlayBounds {
                row: placed.row,
                col: placed.col,
                width: layout.width,
                height: overlay_lines.len(),
            }));
            min_lines_needed = min_lines_needed.max(placed.row + overlay_lines.len());
            rendered.push((entry, overlay_lines, placed.row, placed.col, layout.width));
        }
        self.state.borrow_mut().rendered_overlay_layouts = rendered
            .iter()
            .map(|(entry, lines, row, col, width)| RenderedOverlayLayout {
                entry: entry.clone(),
                row: *row,
                col: *col,
                width: *width,
                height: lines.len(),
            })
            .collect();

        // Pad to the terminal height so overlays get screen-relative positions.
        let working_height = result.len().max(term_height).max(min_lines_needed);
        result.resize(working_height, String::new());
        let viewport_start = working_height.saturating_sub(term_height);
        for (_, overlay_lines, row, col, width) in &rendered {
            for (i, overlay_line) in overlay_lines.iter().enumerate() {
                let index = viewport_start + row + i;
                if index < result.len() {
                    let truncated = if visible_width(overlay_line) > *width {
                        slice_by_column(overlay_line, 0, *width, true)
                    } else {
                        overlay_line.clone()
                    };
                    result[index] = composite_tui_line(&result[index], &truncated, *col, *width, term_width);
                }
            }
        }
        result
    }
}

/// Handle returned by [`crate::Tui::show_overlay`].
#[derive(Clone)]
pub struct OverlayHandle {
    pub(crate) tui: Weak<TuiInner>,
    pub(crate) entry: Rc<OverlayEntry>,
}

impl OverlayHandle {
    /// Removes the overlay for good.
    pub fn hide(&self) {
        if let Some(tui) = self.tui.upgrade() {
            tui.hide_overlay_entry(&self.entry);
        }
    }

    /// Hides or shows the overlay temporarily.
    pub fn set_hidden(&self, hidden: bool) {
        if let Some(tui) = self.tui.upgrade() {
            tui.set_overlay_hidden(&self.entry, hidden);
        }
    }

    pub fn is_hidden(&self) -> bool {
        self.entry.hidden.get()
    }

    /// Focuses the overlay and brings it to the front.
    pub fn focus(&self) {
        if let Some(tui) = self.tui.upgrade() {
            tui.focus_overlay_entry(&self.entry);
        }
    }

    /// Releases focus to the next visible capturing overlay or the component focused before, or
    /// to an explicit target.
    pub fn unfocus(&self, options: Option<OverlayUnfocusOptions>) {
        if let Some(tui) = self.tui.upgrade() {
            tui.unfocus_overlay_entry(&self.entry, options);
        }
    }

    pub fn is_focused(&self) -> bool {
        let Some(tui) = self.tui.upgrade() else { return false };
        let focused = tui.state.borrow().focused.clone();
        focused.is_some_and(|f| crate::component::same_component(&f, &self.entry.component))
    }

    /// Bounds from the last render while the overlay is visible.
    pub fn get_bounds(&self) -> Option<OverlayBounds> {
        let tui = self.tui.upgrade()?;
        let in_stack = tui.state.borrow().overlay_stack.iter().any(|e| Rc::ptr_eq(e, &self.entry));
        if !in_stack || !tui.is_overlay_visible(&self.entry) {
            return None;
        }
        self.entry.bounds.get()
    }

    /// The overlay component.
    pub fn component(&self) -> ComponentRef {
        self.entry.component.clone()
    }
}
