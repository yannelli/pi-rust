//! [`Text`]: multi-line text with word wrapping, padding, and an optional background.

use std::rc::Rc;

use crate::component::Component;
use crate::components::scroll_view::StyleFn;
use crate::line_chunks::LineChunks;
use crate::utils::{apply_background_to_line, flatten_lines, js_trim, visible_width, wrap_text_with_ansi};

/// Displays multi-line text with word wrapping (upstream `Text`).
pub struct Text {
    text: String,
    padding_x: usize,
    padding_y: usize,
    custom_bg_fn: Option<StyleFn>,
    /// Rendered lines and their width. `set_text` and the other setters clear it.
    cache: Option<(usize, Rc<Vec<String>>)>,
}

impl Default for Text {
    /// Upstream `new Text()`: empty text with one column and one row of padding.
    fn default() -> Self {
        Self::new("", 1, 1)
    }
}

impl Text {
    pub fn new(text: &str, padding_x: usize, padding_y: usize) -> Self {
        Self { text: text.to_string(), padding_x, padding_y, custom_bg_fn: None, cache: None }
    }

    /// Upstream `new Text(text)` defaults: one column and one row of padding.
    pub fn plain(text: &str) -> Self {
        Self::new(text, 1, 1)
    }

    /// Builder form of [`Text::set_custom_bg_fn`].
    pub fn with_bg(mut self, bg: impl Fn(&str) -> String + 'static) -> Self {
        self.custom_bg_fn = Some(Rc::new(bg));
        self
    }

    /// Builder form of [`Text::set_custom_bg_fn`] for a shared style function.
    pub fn with_custom_bg_fn(mut self, bg: Option<StyleFn>) -> Self {
        self.custom_bg_fn = bg;
        self
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_string();
        self.cache = None;
    }

    pub fn set_custom_bg_fn(&mut self, bg: Option<StyleFn>) {
        self.custom_bg_fn = bg;
        self.cache = None;
    }

    pub fn set_padding_x(&mut self, padding_x: usize) {
        self.padding_x = padding_x;
        self.cache = None;
    }

    fn background(&self, line: &str, width: usize) -> String {
        match &self.custom_bg_fn {
            Some(bg) => apply_background_to_line(line, width, bg.as_ref()),
            None => format!("{line}{}", " ".repeat(width.saturating_sub(visible_width(line)))),
        }
    }
}

impl Text {
    /// The rendered lines, shared with the cache.
    fn render_shared(&mut self, width: usize) -> Rc<Vec<String>> {
        if let Some((cached_width, lines)) = &self.cache
            && *cached_width == width
        {
            return lines.clone();
        }

        if js_trim(&self.text).is_empty() {
            let lines = Rc::new(Vec::new());
            self.cache = Some((width, lines.clone()));
            return lines;
        }

        let normalized = self.text.replace('\t', "   ");
        // Reduce margins when necessary so content and padding fit within the available width.
        let padding_x = self.padding_x.min(width.saturating_sub(1) / 2);
        let content_width = width.saturating_sub(padding_x * 2).max(1);
        let margin = " ".repeat(padding_x);

        let content: Vec<String> = wrap_text_with_ansi(&normalized, content_width)
            .iter()
            .map(|line| self.background(&format!("{margin}{line}{margin}"), width))
            .collect();
        let empty_line = self.background(&" ".repeat(width), width);

        let mut result = vec![empty_line.clone(); self.padding_y];
        result.extend(content);
        result.extend(std::iter::repeat_n(empty_line, self.padding_y));
        flatten_lines(&mut result);
        let lines = Rc::new(result);
        self.cache = Some((width, lines.clone()));
        if lines.is_empty() { Rc::new(vec![String::new()]) } else { lines }
    }
}

impl Component for Text {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.render_shared(width).as_ref().clone()
    }

    fn render_chunks(&mut self, width: usize, out: &mut LineChunks) {
        out.push(self.render_shared(width));
    }

    fn invalidate(&mut self) {
        self.cache = None;
    }
}
