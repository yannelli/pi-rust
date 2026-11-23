//! Replaces top-level Mermaid code blocks with Unicode diagrams (upstream `components/mermaid.ts`).

use std::sync::Arc;

use pi_tui::components::markdown::{MarkdownCodeBlock, top_level_code_blocks};

use super::{MarkdownMessageType, MarkdownTransformContext, MarkdownTransformer};
use crate::mermaid::{Cls, MermaidArt, Span, render};
use crate::settings::schema::MermaidRenderingMode;
use crate::theme::{ThemeColor, theme};

/// The theme operations diagrams need. [`ActiveThemeStyle`] uses the active theme.
pub trait MermaidStyle {
    fn fg(&self, color: ThemeColor, text: &str) -> String;
    fn bold(&self, text: &str) -> String;
}

/// Styles diagrams with whichever theme is active when they render.
#[derive(Debug, Clone, Copy, Default)]
pub struct ActiveThemeStyle;

impl MermaidStyle for ActiveThemeStyle {
    fn fg(&self, color: ThemeColor, text: &str) -> String {
        theme().fg(color, text)
    }

    fn bold(&self, text: &str) -> String {
        theme().bold(text)
    }
}

impl MermaidStyle for crate::theme::Theme {
    fn fg(&self, color: ThemeColor, text: &str) -> String {
        crate::theme::Theme::fg(self, color, text)
    }

    fn bold(&self, text: &str) -> String {
        crate::theme::Theme::bold(self, text)
    }
}

/// Options for [`create_mermaid_markdown_transformer`].
#[derive(Clone)]
pub struct MermaidTransformerOptions {
    /// The `markdown.mermaid` setting, read on every transform.
    pub get_mode: Arc<dyn Fn() -> MermaidRenderingMode + Send + Sync>,
    /// Colors for the diagram; plain text without one.
    pub theme: Option<Arc<dyn MermaidStyle + Send + Sync>>,
}

/// Skip block lexing unless a line contains both a fence and the literal language name.
fn may_contain_mermaid(markdown: &str) -> bool {
    markdown.split(['\r', '\n']).any(|line| {
        (line.contains("```") || line.contains("~~~"))
            && line.as_bytes().windows(b"mermaid".len()).any(|word| word.eq_ignore_ascii_case(b"mermaid"))
    })
}

fn is_mermaid(info: &str) -> bool {
    info.split_whitespace().next().is_some_and(|lang| lang.eq_ignore_ascii_case("mermaid"))
}

/// One diagram row as inline code, so Markdown keeps its spacing and box-drawing characters.
fn code_span(line: &str) -> String {
    // An empty code span has no height, so blank rows use a non-breaking space.
    let content = if line.is_empty() { "\u{a0}" } else { line };
    let mut longest = 0;
    let mut run = 0;
    for c in content.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat(longest + 1);
    let padding = if content.starts_with('`') || content.ends_with('`') { " " } else { "" };
    format!("{fence}{padding}{content}{padding}{fence}")
}

fn style_span(span: &Span, style: &dyn MermaidStyle) -> String {
    match span.cls {
        Cls::Border => style.fg(ThemeColor::BorderMuted, &span.text),
        Cls::Text => style.fg(ThemeColor::Text, &span.text),
        Cls::Edge => style.fg(ThemeColor::Accent, &span.text),
        Cls::EdgeLabel => style.fg(ThemeColor::Muted, &span.text),
        Cls::Title => style.fg(ThemeColor::Accent, &style.bold(&span.text)),
        Cls::None => span.text.clone(),
    }
}

fn diagram_lines(art: &MermaidArt, style: Option<&dyn MermaidStyle>) -> Vec<String> {
    match style {
        Some(style) => art.styled.iter().map(|row| row.iter().map(|span| style_span(span, style)).collect()).collect(),
        None => art.plain.clone(),
    }
}

fn replacement(
    block: &MarkdownCodeBlock,
    raw: &str,
    context: &MarkdownTransformContext,
    style: Option<&dyn MermaidStyle>,
) -> String {
    let Some(art) = render(&block.text) else { return raw.to_string() };
    if art.width > context.available_width {
        return raw.to_string();
    }
    if !context.is_streaming && !art.warnings.is_empty() {
        let more = art.warnings.len() - 1;
        let suffix = if more > 0 { format!(" (+{more} more)") } else { String::new() };
        let warning = format!("Mermaid diagram not rendered: {}{suffix}", art.warnings[0]);
        let warning = style.map_or(warning.clone(), |style| style.fg(ThemeColor::Warning, &warning));
        return format!("{raw}\n{}  \n", code_span(&warning));
    }
    // Markdown hard breaks keep every diagram row on its own line.
    let rows: Vec<String> = diagram_lines(&art, style).iter().map(|line| code_span(line)).collect();
    format!("{}\n", rows.join("  \n"))
}

/// A transformer that draws top-level Mermaid code blocks as Unicode diagrams. Respects the
/// `markdown.mermaid` mode, skips thinking blocks, and leaves oversized or unsupported diagrams alone.
pub fn create_mermaid_markdown_transformer(options: MermaidTransformerOptions) -> MarkdownTransformer {
    Arc::new(move |markdown: &str, context: &MarkdownTransformContext| {
        let mode = (options.get_mode)();
        if mode == MermaidRenderingMode::Off
            || context.message_type == MarkdownMessageType::AssistantThinking
            || (context.is_streaming && mode != MermaidRenderingMode::Streaming)
            || !may_contain_mermaid(markdown)
        {
            return Some(markdown.to_string());
        }
        let style = options.theme.as_deref().map(|style| style as &dyn MermaidStyle);
        let mut output = String::with_capacity(markdown.len());
        let mut last = 0;
        for block in top_level_code_blocks(markdown) {
            if !is_mermaid(&block.lang) {
                continue;
            }
            let raw = &markdown[block.range.clone()];
            output.push_str(&markdown[last..block.range.start]);
            output.push_str(&replacement(&block, raw, context, style));
            last = block.range.end;
        }
        output.push_str(&markdown[last..]);
        Some(output)
    })
}

#[cfg(test)]
pub(super) fn code_blocks_for_test(markdown: &str) -> Vec<(String, String, String)> {
    top_level_code_blocks(markdown)
        .into_iter()
        .map(|block| (markdown[block.range].to_string(), block.lang, block.text))
        .collect()
}
