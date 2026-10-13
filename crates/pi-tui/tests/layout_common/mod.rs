//! Helpers shared by the layout test files: closure components, stack entries, and frames.
#![allow(dead_code)]

use std::cell::RefCell;
use std::rc::Rc;

use pi_tui::layout::{LayoutFrame, render_layout_frame};
use pi_tui::layout_node::{LayoutViewport, StackBasis};
use pi_tui::utils::strip_terminal_sequences;
use pi_tui::{Component, ComponentRef, StackChild, StackEntryOptions, component};

use crate::support::Text;

/// A component that renders with a closure (upstream object literal components).
pub struct FnComponent(Box<dyn FnMut(usize) -> Vec<String>>);

impl Component for FnComponent {
    fn render(&mut self, width: usize) -> Vec<String> {
        (self.0)(width)
    }
}

pub fn fn_component(render: impl FnMut(usize) -> Vec<String> + 'static) -> ComponentRef {
    component(FnComponent(Box::new(render)))
}

/// Upstream `new Text(text, 0, 0)`.
pub fn text(value: &str) -> Rc<RefCell<Text>> {
    component(Text::new(value, 0, 0))
}

/// Builder for a stack entry with sizing rules.
pub struct EntryBuilder(ComponentRef, StackEntryOptions);

pub fn entry(component: ComponentRef) -> EntryBuilder {
    EntryBuilder(component, StackEntryOptions::default())
}

impl EntryBuilder {
    pub fn basis(mut self, size: usize) -> Self {
        self.1.basis = Some(StackBasis::Fixed(size));
        self
    }

    pub fn auto(mut self) -> Self {
        self.1.basis = Some(StackBasis::Auto);
        self
    }

    pub fn grow(mut self, grow: usize) -> Self {
        self.1.grow = Some(grow);
        self
    }

    pub fn shrink(mut self, shrink: usize) -> Self {
        self.1.shrink = Some(shrink);
        self
    }

    pub fn min(mut self, min_size: usize) -> Self {
        self.1.min_size = Some(min_size);
        self
    }

    pub fn visible(mut self, visible: impl Fn(&LayoutViewport) -> bool + 'static) -> Self {
        self.1.visible = Some(Rc::new(visible));
        self
    }
}

impl From<EntryBuilder> for StackChild {
    fn from(builder: EntryBuilder) -> Self {
        StackChild::Entry(builder.0, builder.1)
    }
}

/// Upstream `renderLayoutFrame(root, width, height, () => {})`.
pub fn frame(root: &ComponentRef, width: usize, height: usize) -> LayoutFrame {
    render_layout_frame(root, width, height, Rc::new(|| {}))
}

/// Lines without escape sequences and trailing spaces.
pub fn visible_lines(lines: &[String]) -> Vec<String> {
    lines.iter().map(|line| strip_terminal_sequences(line).trim_end().to_string()).collect()
}

/// Lines without escape sequences.
pub fn stripped(lines: &[String]) -> Vec<String> {
    lines.iter().map(|line| strip_terminal_sequences(line)).collect()
}

/// Numbered lines joined with newlines: `"1\n2\n..."`.
pub fn numbered(count: usize, prefix: &str) -> String {
    (1..=count).map(|index| format!("{prefix}{index}")).collect::<Vec<_>>().join("\n")
}
