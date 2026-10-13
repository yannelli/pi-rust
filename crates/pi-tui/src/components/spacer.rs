//! [`Spacer`]: a fixed number of empty lines.

use crate::component::Component;

/// Renders `lines` empty lines (upstream `Spacer`).
pub struct Spacer {
    lines: usize,
}

impl Default for Spacer {
    /// Upstream `new Spacer()`: one empty line.
    fn default() -> Self {
        Self::new(1)
    }
}

impl Spacer {
    pub fn new(lines: usize) -> Self {
        Self { lines }
    }

    pub fn set_lines(&mut self, lines: usize) {
        self.lines = lines;
    }
}

impl Component for Spacer {
    fn render(&mut self, _width: usize) -> Vec<String> {
        vec![String::new(); self.lines]
    }
}
