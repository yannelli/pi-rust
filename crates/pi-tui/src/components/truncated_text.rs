//! [`TruncatedText`]: a single line of text truncated to the viewport width.

use crate::component::Component;
use crate::utils::{truncate_to_width, visible_width};

/// Shows the first line of its text, truncated with an ellipsis and padded to the full width
/// (upstream `TruncatedText`).
pub struct TruncatedText {
    text: String,
    padding_x: usize,
    padding_y: usize,
}

impl TruncatedText {
    pub fn new(text: &str, padding_x: usize, padding_y: usize) -> Self {
        Self { text: text.to_string(), padding_x, padding_y }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_string();
    }
}

impl Component for TruncatedText {
    fn render(&mut self, width: usize) -> Vec<String> {
        let empty_line = " ".repeat(width);
        let mut result = vec![empty_line.clone(); self.padding_y];

        let available_width = width.saturating_sub(self.padding_x * 2).max(1);
        let single_line = self.text.split('\n').next().unwrap_or_default();
        let display_text = truncate_to_width(single_line, available_width);

        let padding = " ".repeat(self.padding_x);
        let line = format!("{padding}{display_text}{padding}");
        let fill = width.saturating_sub(visible_width(&line));
        result.push(format!("{line}{}", " ".repeat(fill)));

        result.extend(std::iter::repeat_n(empty_line, self.padding_y));
        result
    }
}
