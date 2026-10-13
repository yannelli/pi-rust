//! [`BoxComponent`]: a container that applies padding and a background to its children
//! (upstream `Box`, renamed so it does not shadow `std::boxed::Box`).

use crate::component::{
    Component, ComponentRef, MouseResponse, TuiMouseEvent, dispatch_mouse_event, invalidate_all, same_component,
};
use crate::components::scroll_view::StyleFn;
use crate::utils::{flatten_lines, visible_width};

struct RenderCache {
    child_lines: Vec<String>,
    width: usize,
    bg_sample: Option<String>,
    lines: Vec<String>,
}

/// Renders children with horizontal and vertical padding and an optional background function.
pub struct BoxComponent {
    pub children: Vec<ComponentRef>,
    padding_x: usize,
    padding_y: usize,
    bg_fn: Option<StyleFn>,
    cache: Option<RenderCache>,
    mouse_layout: Option<(usize, Vec<(ComponentRef, usize)>)>,
}

impl Default for BoxComponent {
    /// Upstream `new Box()`: one column and one row of padding, no background.
    fn default() -> Self {
        Self::new(1, 1, None)
    }
}

impl BoxComponent {
    pub fn new(padding_x: usize, padding_y: usize, bg_fn: Option<StyleFn>) -> Self {
        Self { children: Vec::new(), padding_x, padding_y, bg_fn, cache: None, mouse_layout: None }
    }

    pub fn add_child(&mut self, component: ComponentRef) {
        self.children.push(component);
        self.cache = None;
    }

    pub fn remove_child(&mut self, component: &ComponentRef) {
        if let Some(index) = self.children.iter().position(|c| same_component(c, component)) {
            self.children.remove(index);
            self.cache = None;
        }
    }

    pub fn clear(&mut self) {
        self.children.clear();
        self.cache = None;
    }

    /// Replaces the background. Changes are detected by sampling the output, so the cache stays.
    pub fn set_bg_fn(&mut self, bg_fn: Option<StyleFn>) {
        self.bg_fn = bg_fn;
    }

    pub fn set_padding_x(&mut self, padding_x: usize) {
        self.padding_x = padding_x;
        self.cache = None;
    }

    fn content_width(&self, width: usize) -> usize {
        width.saturating_sub(self.padding_x * 2).max(1)
    }

    fn apply_bg(&self, line: &str, width: usize) -> String {
        let padded = format!("{line}{}", " ".repeat(width.saturating_sub(visible_width(line))));
        match &self.bg_fn {
            Some(bg) => bg(&padded),
            None => padded,
        }
    }
}

impl Component for BoxComponent {
    fn render(&mut self, width: usize) -> Vec<String> {
        if self.children.is_empty() {
            return Vec::new();
        }
        let content_width = self.content_width(width);
        let mut child_lines = Vec::new();
        let mut heights = Vec::with_capacity(self.children.len());
        for child in &self.children {
            let lines = child.borrow_mut().render(content_width);
            heights.push((child.clone(), lines.len()));
            child_lines.extend(lines);
        }
        self.mouse_layout = Some((content_width, heights));
        if child_lines.is_empty() {
            return Vec::new();
        }

        let bg_sample = self.bg_fn.as_ref().map(|bg| bg("test"));
        if let Some(cache) = &self.cache
            && cache.width == width
            && cache.bg_sample == bg_sample
            && cache.child_lines == child_lines
        {
            return cache.lines.clone();
        }

        let left_pad = " ".repeat(self.padding_x);
        let empty = self.apply_bg("", width);
        let mut result = vec![empty.clone(); self.padding_y];
        result.extend(child_lines.iter().map(|line| self.apply_bg(&format!("{left_pad}{line}"), width)));
        result.extend(std::iter::repeat_n(empty, self.padding_y));
        flatten_lines(&mut result);
        self.cache = Some(RenderCache { child_lines, width, bg_sample, lines: result.clone() });
        result
    }

    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<MouseResponse> {
        let content_width = self.content_width(event.width.max(0) as usize);
        let content_y = event.y - self.padding_y as i32;
        let content_x = event.x - self.padding_x as i32;
        if content_y < 0 || content_x < 0 || content_x >= content_width as i32 {
            return None;
        }
        let heights: Vec<(ComponentRef, usize)> = match &self.mouse_layout {
            Some((layout_width, heights)) if *layout_width == content_width => heights.clone(),
            _ => self.children.iter().map(|c| (c.clone(), c.borrow_mut().render(content_width).len())).collect(),
        };
        let mut child_y = 0i32;
        for (child, height) in heights {
            let height = height as i32;
            if content_y >= child_y && content_y < child_y + height {
                let local = TuiMouseEvent {
                    x: content_x,
                    y: content_y - child_y,
                    width: content_width as i32,
                    height,
                    ..event.clone()
                };
                return dispatch_mouse_event(&child, &local).map(MouseResponse::Forwarded);
            }
            child_y += height;
        }
        None
    }

    fn invalidate(&mut self) {
        self.cache = None;
        invalidate_all(&self.children);
    }
}
