//! Replaces top-level Mermaid code blocks with Unicode diagrams (upstream `components/mermaid.ts`).

use std::ops::Range;
use std::sync::Arc;

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

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

/// A top-level fenced code block: its source extent (as marked's token `raw`), info string, and text.
struct CodeBlock {
    raw: Range<usize>,
    info: String,
    text: String,
}

/// marked appends a single following newline to a block's `raw`; longer blank runs stay separate.
fn marked_raw_end(markdown: &str, block_end: usize) -> usize {
    let core = markdown[..block_end].strip_suffix('\n').map_or(block_end, str::len);
    let rest = &markdown[core..];
    let mut blank = 0;
    for line in rest.split_inclusive('\n') {
        if !line.trim_matches([' ', '\t']).trim_end_matches('\n').is_empty() {
            break;
        }
        blank += line.len();
        if !line.ends_with('\n') {
            break;
        }
    }
    if blank == 1 { core + 1 } else { core }
}

fn top_level_code_blocks(markdown: &str) -> Vec<CodeBlock> {
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut blocks = Vec::new();
    let mut depth = 0usize;
    let mut current: Option<CodeBlock> = None;
    for (event, range) in Parser::new_ext(markdown, options).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info))) if depth == 0 => {
                current = Some(CodeBlock { raw: range.start..range.end, info: info.to_string(), text: String::new() });
                depth += 1;
            }
            Event::Start(_) => depth += 1,
            Event::Text(text) if depth == 1 => {
                if let Some(block) = current.as_mut() {
                    block.text.push_str(&text);
                }
            }
            Event::End(TagEnd::CodeBlock) if depth == 1 => {
                depth -= 1;
                if let Some(mut block) = current.take() {
                    block.raw.end = marked_raw_end(markdown, block.raw.end);
                    if block.text.ends_with('\n') {
                        block.text.pop();
                    }
                    blocks.push(block);
                }
            }
            Event::End(_) => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    blocks
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
    block: &CodeBlock,
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
        {
            return Some(markdown.to_string());
        }
        let style = options.theme.as_deref().map(|style| style as &dyn MermaidStyle);
        let mut output = String::with_capacity(markdown.len());
        let mut last = 0;
        for block in top_level_code_blocks(markdown) {
            if !is_mermaid(&block.info) {
                continue;
            }
            let raw = &markdown[block.raw.clone()];
            output.push_str(&markdown[last..block.raw.start]);
            output.push_str(&replacement(&block, raw, context, style));
            last = block.raw.end;
        }
        output.push_str(&markdown[last..]);
        Some(output)
    })
}

#[cfg(test)]
pub(super) fn code_blocks_for_test(markdown: &str) -> Vec<(String, String, String)> {
    top_level_code_blocks(markdown)
        .into_iter()
        .map(|block| (markdown[block.raw].to_string(), block.info, block.text))
        .collect()
}
