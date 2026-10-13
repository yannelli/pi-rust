//! [`DynamicBorder`]: a horizontal rule that spans the viewport (upstream `dynamic-border.ts`).

use std::rc::Rc;

use pi_tui::Component;

use crate::theme::{ThemeColor, theme};

/// Styles border text.
pub type BorderColorFn = Rc<dyn Fn(&str) -> String>;

/// A `─` line as wide as the viewport, in the theme's `border` color unless a color is given.
pub struct DynamicBorder {
    color: BorderColorFn,
}

impl Default for DynamicBorder {
    fn default() -> Self {
        Self::new()
    }
}

impl DynamicBorder {
    pub fn new() -> Self {
        Self { color: Rc::new(|text: &str| theme().fg(ThemeColor::Border, text)) }
    }

    pub fn with_color(color: impl Fn(&str) -> String + 'static) -> Self {
        Self { color: Rc::new(color) }
    }
}

impl Component for DynamicBorder {
    fn render(&mut self, width: usize) -> Vec<String> {
        vec![(self.color)(&"─".repeat(width.max(1)))]
    }
}
