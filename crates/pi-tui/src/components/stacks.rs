//! [`VStack`] and [`HStack`]: stacks that render standalone and lay out in fullscreen mode.

use std::ops::{Deref, DerefMut};

use super::stack::{Stack, StackChild, StackOptions, allocate_stack_sizes, visible_stack_entries};
use crate::component::{
    Component, ComponentRef, MouseResponse, TuiMouseEvent, dispatch_children_mouse, invalidate_all,
};
use crate::layout_node::{LayoutNode, LayoutViewport, StackAlign, StackDirection};
use crate::tui::composite_tui_line;
use crate::utils::visible_width;

/// Children stacked top to bottom.
pub struct VStack(Stack);

impl VStack {
    pub fn new(children: Vec<StackChild>, options: StackOptions) -> Self {
        Self(Stack::new(StackDirection::Vertical, children, options))
    }
}

impl Default for VStack {
    fn default() -> Self {
        Self::new(Vec::new(), StackOptions::default())
    }
}

/// Children placed left to right.
pub struct HStack(Stack);

impl HStack {
    pub fn new(children: Vec<StackChild>, options: StackOptions) -> Self {
        Self(Stack::new(StackDirection::Horizontal, children, options))
    }
}

impl Default for HStack {
    fn default() -> Self {
        Self::new(Vec::new(), StackOptions::default())
    }
}

macro_rules! stack_deref {
    ($name:ident) => {
        impl Deref for $name {
            type Target = Stack;
            fn deref(&self) -> &Stack {
                &self.0
            }
        }
        impl DerefMut for $name {
            fn deref_mut(&mut self) -> &mut Stack {
                &mut self.0
            }
        }
    };
}

stack_deref!(VStack);
stack_deref!(HStack);

fn unbounded_viewport(width: usize) -> LayoutViewport {
    LayoutViewport { width: width.max(1), height: usize::MAX }
}

impl Component for VStack {
    fn render(&mut self, width: usize) -> Vec<String> {
        let viewport = unbounded_viewport(width);
        let entries = visible_stack_entries(&self.0.entries, &viewport);
        let rendered: Vec<Vec<String>> =
            entries.iter().map(|e| e.component.borrow_mut().render(viewport.width)).collect();
        let heights: Vec<usize> = rendered.iter().map(Vec::len).collect();
        let sizes = allocate_stack_sizes(&entries, &heights, None, self.0.gap);
        let mut lines = Vec::new();
        for (index, child_lines) in rendered.into_iter().enumerate() {
            if index > 0 {
                lines.extend(std::iter::repeat_n(String::new(), self.0.gap));
            }
            let size = sizes[index];
            let count = child_lines.len().min(size);
            lines.extend(child_lines.into_iter().take(size));
            lines.extend(std::iter::repeat_n(String::new(), size - count));
        }
        lines
    }

    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<MouseResponse> {
        dispatch_children_mouse(&self.0.container.children, None, event).map(MouseResponse::Forwarded)
    }

    fn invalidate(&mut self) {
        invalidate_all(&self.0.container.children);
    }

    fn container_children(&self) -> Option<Vec<ComponentRef>> {
        Some(self.0.container.children.clone())
    }

    fn layout_node(&self) -> Option<LayoutNode> {
        Some(LayoutNode::Stack(self.0.layout_node()))
    }
}

impl Component for HStack {
    fn render(&mut self, width: usize) -> Vec<String> {
        let safe_width = width.max(1);
        let viewport = unbounded_viewport(safe_width);
        let entries = visible_stack_entries(&self.0.entries, &viewport);
        if entries.is_empty() {
            return Vec::new();
        }
        let intrinsic: Vec<usize> = entries
            .iter()
            .map(|e| e.component.borrow_mut().render(safe_width).iter().map(|l| visible_width(l)).max().unwrap_or(0))
            .collect();
        let widths = allocate_stack_sizes(&entries, &intrinsic, Some(safe_width), self.0.gap);
        let rendered: Vec<Vec<String>> = entries
            .iter()
            .zip(&widths)
            .map(|(e, &w)| if w == 0 { Vec::new() } else { e.component.borrow_mut().render(w) })
            .collect();
        let height = rendered.iter().map(Vec::len).max().unwrap_or(0);
        let mut result = vec![String::new(); height];
        let mut x = 0;
        for (lines, &child_width) in rendered.iter().zip(&widths) {
            let offset = match self.0.align {
                StackAlign::Center => (height - lines.len()) / 2,
                StackAlign::End => height - lines.len(),
                StackAlign::Stretch | StackAlign::Start => 0,
            };
            for (row, line) in lines.iter().enumerate() {
                let target = row + offset;
                if target < result.len() {
                    result[target] = composite_tui_line(&result[target], line, x, child_width, safe_width);
                }
            }
            x += child_width + self.0.gap;
        }
        result
    }

    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<MouseResponse> {
        dispatch_children_mouse(&self.0.container.children, None, event).map(MouseResponse::Forwarded)
    }

    fn invalidate(&mut self) {
        invalidate_all(&self.0.container.children);
    }

    fn container_children(&self) -> Option<Vec<ComponentRef>> {
        Some(self.0.container.children.clone())
    }

    fn layout_node(&self) -> Option<LayoutNode> {
        Some(LayoutNode::Stack(self.0.layout_node()))
    }
}
