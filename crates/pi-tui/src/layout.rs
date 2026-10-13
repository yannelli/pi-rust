//! Fullscreen layout engine: lays out stacks and scroll views into a frame of terminal rows
//! (upstream `layout.ts`).

mod paint;

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::LazyLock;

use regex::Regex;

pub use paint::{ScrollbarGeometry, get_scrollbar_geometry};

use crate::component::CURSOR_MARKER;
use crate::component::{ComponentRef, component_key};
use crate::components::scroll_view::ScrollViewHandle;
use crate::components::stack::{allocate_stack_sizes, visible_stack_entries};
use crate::layout_node::{LayoutNode, LayoutViewport, StackAlign, StackDirection, get_layout_node};
use crate::utils::visible_width;

pub(crate) static OSC133_ZONE_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:\x1b\]133;[ABC](?:\x07|\x1b\\))+").expect("valid regex"));

/// A rectangle in terminal cells. Coordinates may be negative for scrolled-out content.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LayoutRect {
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
}

impl LayoutRect {
    pub fn contains(&self, x: i64, y: i64) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }
}

fn intersect(a: LayoutRect, b: LayoutRect) -> LayoutRect {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let right = (a.x + a.width).min(b.x + b.width);
    let bottom = (a.y + a.height).min(b.y + b.height);
    LayoutRect { x, y, width: (right - x).max(0), height: (bottom - y).max(0) }
}

/// One laid-out component.
#[derive(Clone)]
pub struct LayoutBox {
    pub component: ComponentRef,
    pub rect: LayoutRect,
    /// Visible part of `rect` after clipping by ancestors.
    pub clip: LayoutRect,
    pub children: Vec<LayoutBox>,
    /// Rendered lines of a leaf component.
    pub lines: Option<Rc<Vec<String>>>,
    /// First line shown when a leaf is taller than its box (keeps the cursor line visible).
    pub line_offset: usize,
    /// Scroll state when this box is a scroll view.
    pub scroll_view: Option<ScrollViewHandle>,
    /// Full content lines of a scroll view.
    pub scroll_content_lines: Option<Rc<Vec<String>>>,
    pub layer: i32,
}

/// A laid-out and painted frame.
#[derive(Clone)]
pub struct LayoutFrame {
    pub root: LayoutBox,
    pub width: usize,
    pub height: usize,
    /// One line per terminal row.
    pub lines: Vec<String>,
    pub primary_scroll_view: Option<ScrollViewHandle>,
}

struct LayoutContext {
    viewport: LayoutViewport,
    render_cache: HashMap<(usize, usize), Rc<Vec<String>>>,
    request_render: Rc<dyn Fn()>,
    primary_scroll_view: Option<ScrollViewHandle>,
}

impl LayoutContext {
    fn render_cached(&mut self, component: &ComponentRef, width: i64) -> Rc<Vec<String>> {
        let safe_width = width.max(1) as usize;
        let key = (component_key(component), safe_width);
        if let Some(lines) = self.render_cache.get(&key) {
            return lines.clone();
        }
        let lines = Rc::new(component.borrow_mut().render(safe_width));
        self.render_cache.insert(key, lines.clone());
        lines
    }

    fn measure_height(&mut self, component: &ComponentRef, width: i64) -> i64 {
        self.render_cached(component, width).len() as i64
    }

    fn measure_width(&mut self, component: &ComponentRef, width: i64) -> i64 {
        self.render_cached(component, width).iter().map(|line| visible_width(line) as i64).max().unwrap_or(0)
    }
}

fn translate_box(layout_box: &mut LayoutBox, delta_y: i64) {
    layout_box.rect.y += delta_y;
    for child in &mut layout_box.children {
        translate_box(child, delta_y);
    }
}

fn update_clips(layout_box: &mut LayoutBox, parent_clip: LayoutRect) {
    layout_box.clip = intersect(parent_clip, layout_box.rect);
    let clip = layout_box.clip;
    for child in &mut layout_box.children {
        update_clips(child, clip);
    }
}

fn leaf_box(component: &ComponentRef, rect: LayoutRect, clip: LayoutRect) -> LayoutBox {
    LayoutBox {
        component: component.clone(),
        rect,
        clip,
        children: Vec::new(),
        lines: None,
        line_offset: 0,
        scroll_view: None,
        scroll_content_lines: None,
        layer: 0,
    }
}

fn layout_component(
    context: &mut LayoutContext,
    component: &ComponentRef,
    x: i64,
    y: i64,
    width: i64,
    height: Option<i64>,
    clip: LayoutRect,
) -> LayoutBox {
    let safe_width = width.max(1);
    let Some(node) = get_layout_node(component) else {
        let lines = context.render_cached(component, safe_width);
        let allocated = height.map_or(lines.len() as i64, |h| h.max(0));
        let mut line_offset = 0;
        if lines.len() as i64 > allocated && allocated > 0 {
            let cursor_line = lines.iter().position(|line| line.contains(CURSOR_MARKER));
            if let Some(cursor_line) = cursor_line.filter(|&l| l as i64 >= allocated) {
                line_offset = cursor_line + 1 - allocated as usize;
            }
        }
        let rect = LayoutRect { x, y, width: safe_width, height: allocated };
        let mut layout_box = leaf_box(component, rect, intersect(clip, rect));
        layout_box.lines = Some(lines);
        layout_box.line_offset = line_offset;
        return layout_box;
    };

    match node {
        LayoutNode::Scroll(scroll) => {
            let state = scroll.state;
            let previous_top = state.scroll_top() as i64;
            let content_width = state.get_content_width(safe_width as usize) as i64;
            let mut child =
                layout_component(context, &scroll.component, x, y - previous_top, content_width, None, clip);
            let content_height = child.rect.height;
            let viewport_height = height.map_or(content_height, |h| h.max(0));
            state.update_layout(content_height as usize, viewport_height as usize, context.request_render.clone());
            translate_box(&mut child, previous_top - state.scroll_top() as i64);
            if state.primary() || context.primary_scroll_view.is_none() {
                context.primary_scroll_view = Some(state.clone());
            }
            let rect = LayoutRect { x, y, width: safe_width, height: viewport_height };
            let child_clip = intersect(clip, rect);
            update_clips(&mut child, child_clip);
            let content_lines = context.render_cached(&scroll.component, content_width);
            let mut layout_box = leaf_box(component, rect, child_clip);
            layout_box.children.push(child);
            layout_box.scroll_view = Some(state);
            layout_box.scroll_content_lines = Some(content_lines);
            layout_box
        }
        LayoutNode::Stack(stack) => {
            let entries = visible_stack_entries(&stack.entries, &context.viewport);
            let gap = stack.gap as i64;
            let gap_total = entries.len().saturating_sub(1) as i64 * gap;
            if stack.direction == StackDirection::Vertical {
                let intrinsic: Vec<usize> = entries
                    .iter()
                    .map(|entry| match entry.basis {
                        Some(crate::layout_node::StackBasis::Fixed(size)) => size,
                        _ => context.measure_height(&entry.component, safe_width) as usize,
                    })
                    .collect();
                let sizes = allocate_stack_sizes(&entries, &intrinsic, height.map(|h| h.max(0) as usize), stack.gap);
                let natural = sizes.iter().sum::<usize>() as i64 + gap_total;
                let allocated = height.map_or(natural, |h| h.max(0));
                let rect = LayoutRect { x, y, width: safe_width, height: allocated };
                let mut layout_box = leaf_box(component, rect, intersect(clip, rect));
                let mut child_y = y;
                for (entry, size) in entries.iter().zip(&sizes) {
                    let child = layout_component(
                        context,
                        &entry.component,
                        x,
                        child_y,
                        safe_width,
                        Some(*size as i64),
                        layout_box.clip,
                    );
                    layout_box.children.push(child);
                    child_y += *size as i64 + gap;
                }
                return layout_box;
            }

            let intrinsic: Vec<usize> = entries
                .iter()
                .map(|entry| match entry.basis {
                    Some(crate::layout_node::StackBasis::Fixed(size)) => size,
                    _ => context.measure_width(&entry.component, safe_width) as usize,
                })
                .collect();
            let widths = allocate_stack_sizes(&entries, &intrinsic, Some(safe_width as usize), stack.gap);
            let heights: Vec<i64> = entries
                .iter()
                .zip(&widths)
                .map(|(entry, &w)| context.measure_height(&entry.component, (w as i64).max(1)))
                .collect();
            let allocated = height.map_or_else(|| heights.iter().copied().max().unwrap_or(0), |h| h.max(0));
            let rect = LayoutRect { x, y, width: safe_width, height: allocated };
            let mut layout_box = leaf_box(component, rect, intersect(clip, rect));
            let mut child_x = x;
            for ((entry, &child_width), &natural) in entries.iter().zip(&widths).zip(&heights) {
                let child_height = if stack.align == StackAlign::Stretch { allocated } else { allocated.min(natural) };
                let child_y = match stack.align {
                    StackAlign::Center => y + (allocated - child_height).div_euclid(2),
                    StackAlign::End => y + allocated - child_height,
                    StackAlign::Stretch | StackAlign::Start => y,
                };
                let child_width = child_width as i64;
                if child_width == 0 {
                    let rect = LayoutRect { x: child_x, y: child_y, width: 0, height: child_height };
                    let clip = LayoutRect { x: child_x, y: child_y, width: 0, height: 0 };
                    layout_box.children.push(leaf_box(&entry.component, rect, clip));
                } else {
                    let child = layout_component(
                        context,
                        &entry.component,
                        child_x,
                        child_y,
                        child_width,
                        Some(child_height),
                        layout_box.clip,
                    );
                    layout_box.children.push(child);
                }
                child_x += child_width + gap;
            }
            layout_box
        }
    }
}

/// Lays out `root` into a `width` x `height` frame and paints it.
pub fn render_layout_frame(
    root: &ComponentRef,
    width: usize,
    height: usize,
    request_render: Rc<dyn Fn()>,
) -> LayoutFrame {
    let safe_width = width.max(1);
    let safe_height = height.max(1);
    let mut context = LayoutContext {
        viewport: LayoutViewport { width: safe_width, height: safe_height },
        render_cache: HashMap::new(),
        request_render,
        primary_scroll_view: None,
    };
    let bounds = LayoutRect { x: 0, y: 0, width: safe_width as i64, height: safe_height as i64 };
    let root_box = layout_component(&mut context, root, 0, 0, safe_width as i64, Some(safe_height as i64), bounds);
    let mut lines = vec![String::new(); safe_height];
    paint::paint_box(&root_box, &mut lines, safe_width);
    LayoutFrame {
        root: root_box,
        width: safe_width,
        height: safe_height,
        lines,
        primary_scroll_view: context.primary_scroll_view,
    }
}

/// Visual hit path at a cell, from the deepest component up to the layout root.
pub fn get_layout_boxes_at(frame: &LayoutFrame, x: i64, y: i64) -> Vec<&LayoutBox> {
    fn visit<'a>(layout_box: &'a LayoutBox, depth: usize, x: i64, y: i64, out: &mut Vec<(&'a LayoutBox, usize)>) {
        if !layout_box.clip.contains(x, y) {
            return;
        }
        out.push((layout_box, depth));
        for child in &layout_box.children {
            visit(child, depth + 1, x, y, out);
        }
    }
    let mut result = Vec::new();
    visit(&frame.root, 0, x, y, &mut result);
    result.sort_by(|a, b| b.0.layer.cmp(&a.0.layer).then(b.1.cmp(&a.1)));
    result.into_iter().map(|(layout_box, _)| layout_box).collect()
}

/// The box of a scroll view in a frame.
pub fn get_scroll_view_box<'a>(frame: &'a LayoutFrame, scroll_view: &ScrollViewHandle) -> Option<&'a LayoutBox> {
    fn visit<'a>(layout_box: &'a LayoutBox, target: &ScrollViewHandle) -> Option<&'a LayoutBox> {
        if layout_box.scroll_view.as_ref().is_some_and(|s| s.ptr_eq(target)) {
            return Some(layout_box);
        }
        layout_box.children.iter().find_map(|child| visit(child, target))
    }
    visit(&frame.root, scroll_view)
}

/// Scroll views under a cell, innermost first.
pub fn get_scroll_views_at(frame: &LayoutFrame, x: i64, y: i64) -> Vec<ScrollViewHandle> {
    fn visit(layout_box: &LayoutBox, depth: usize, x: i64, y: i64, out: &mut Vec<(ScrollViewHandle, usize)>) {
        if !layout_box.clip.contains(x, y) {
            return;
        }
        if let Some(scroll_view) = &layout_box.scroll_view
            && layout_box.rect.contains(x, y)
        {
            out.push((scroll_view.clone(), depth));
        }
        for child in &layout_box.children {
            visit(child, depth + 1, x, y, out);
        }
    }
    let mut result = Vec::new();
    visit(&frame.root, 0, x, y, &mut result);
    result.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    result.into_iter().map(|(scroll_view, _)| scroll_view).collect()
}
