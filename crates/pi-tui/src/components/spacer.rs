//! [`Spacer`]: a fixed number of empty lines.

use std::rc::Rc;

use crate::component::Component;
use crate::line_chunks::LineChunks;

/// Renders `lines` empty lines (upstream `Spacer`).
pub struct Spacer {
    lines: usize,
    shared: Option<Rc<Vec<String>>>,
}

impl Default for Spacer {
    /// Upstream `new Spacer()`: one empty line.
    fn default() -> Self {
        Self::new(1)
    }
}

impl Spacer {
    pub fn new(lines: usize) -> Self {
        Self { lines, shared: None }
    }

    pub fn set_lines(&mut self, lines: usize) {
        self.lines = lines;
    }
}

impl Component for Spacer {
    fn render(&mut self, _width: usize) -> Vec<String> {
        vec![String::new(); self.lines]
    }

    fn render_chunks(&mut self, _width: usize, out: &mut LineChunks) {
        let lines = self.lines;
        let shared = self.shared.get_or_insert_with(|| Rc::new(vec![String::new(); lines]));
        if shared.len() != lines {
            *shared = Rc::new(vec![String::new(); lines]);
        }
        out.push(shared.clone());
    }
}
