//! Truncating text to visual lines, accounting for wrapping (upstream `visual-truncate.ts`).

use pi_tui::utils::truncate_to_width_with;
use pi_tui::{Component, Text};

/// Which visual lines to keep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keep {
    Start,
    End,
}

/// The visual lines to show and how many were skipped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VisualTruncateResult {
    pub visual_lines: Vec<String>,
    pub skipped_count: usize,
}

/// Wrap `text` at `width` with `padding_x` and keep at most `max_visual_lines` of the result.
pub fn truncate_to_visual_lines(
    text: &str,
    max_visual_lines: usize,
    width: usize,
    padding_x: usize,
    keep: Keep,
) -> VisualTruncateResult {
    if text.is_empty() {
        return VisualTruncateResult::default();
    }
    let all = Text::new(text, padding_x, 0).render(width);
    if all.len() <= max_visual_lines {
        return VisualTruncateResult { visual_lines: all, skipped_count: 0 };
    }
    let skipped_count = all.len() - max_visual_lines;
    let visual_lines = match keep {
        Keep::Start => all[..max_visual_lines].to_vec(),
        Keep::End => all[all.len() - max_visual_lines..].to_vec(),
    };
    VisualTruncateResult { visual_lines, skipped_count }
}

/// Collapsed output cut to `max_visual_lines` visual lines, with a hint for the hidden ones.
/// Caches its lines per width, since it renders on every frame.
pub struct VisualLinePreview {
    text: String,
    max_visual_lines: usize,
    keep: Keep,
    format_hint: Box<dyn Fn(usize) -> String>,
    cache: Option<(usize, Vec<String>)>,
}

impl VisualLinePreview {
    pub fn new(
        text: impl Into<String>,
        max_visual_lines: usize,
        keep: Keep,
        format_hint: impl Fn(usize) -> String + 'static,
    ) -> Self {
        Self { text: text.into(), max_visual_lines, keep, format_hint: Box::new(format_hint), cache: None }
    }
}

impl Component for VisualLinePreview {
    fn render(&mut self, width: usize) -> Vec<String> {
        if let Some((cached_width, lines)) = &self.cache
            && *cached_width == width
        {
            return lines.clone();
        }
        let preview = truncate_to_visual_lines(&self.text, self.max_visual_lines, width, 0, self.keep);
        let mut lines = preview.visual_lines;
        if preview.skipped_count > 0 {
            let hint = truncate_to_width_with(&(self.format_hint)(preview.skipped_count), width, "...", false);
            match self.keep {
                Keep::Start => lines.push(hint),
                Keep::End => lines.insert(0, hint),
            }
        }
        self.cache = Some((width, lines.clone()));
        lines
    }

    fn invalidate(&mut self) {
        self.cache = None;
    }
}
