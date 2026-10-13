//! [`Markdown`]: renders Markdown (GFM tables, task lists, code blocks, links, block quotes, and
//! LaTeX math) as styled terminal lines. Parsing follows `marked` 18 like upstream.

mod block;
mod inline;
mod latex_ext;
mod lexer;
mod list;
mod render;
mod rules;
mod table;
mod token;

use std::rc::Rc;

use crate::component::Component;
use crate::components::scroll_view::StyleFn;
use crate::terminal_image::is_image_line;
use crate::utils::{apply_background_to_line, flatten_lines, js_trim, visible_width, wrap_text_with_ansi};
use token::Token;

/// Syntax highlighting hook: code and optional language to styled lines.
pub type HighlightCodeFn = Rc<dyn Fn(&str, Option<&str>) -> Vec<String>>;

/// Source transform hook: Markdown and the exact width available for content to new Markdown.
pub type MarkdownTransformFn = Rc<dyn Fn(&str, usize) -> String>;

/// Styling of Markdown elements. Each function wraps text in ANSI codes.
#[derive(Clone)]
pub struct MarkdownTheme {
    pub heading: StyleFn,
    pub link: StyleFn,
    pub link_url: StyleFn,
    pub code: StyleFn,
    pub code_block: StyleFn,
    pub code_block_border: StyleFn,
    pub quote: StyleFn,
    pub quote_border: StyleFn,
    pub hr: StyleFn,
    pub list_bullet: StyleFn,
    pub bold: StyleFn,
    pub italic: StyleFn,
    pub strikethrough: StyleFn,
    pub underline: StyleFn,
    /// Highlights code blocks. Without it, each line is styled with `code_block`.
    pub highlight_code: Option<HighlightCodeFn>,
    /// Prefix of each rendered code block line (default two spaces).
    pub code_block_indent: Option<String>,
}

/// Text styling applied to all Markdown content unless formatting overrides it.
#[derive(Clone, Default)]
pub struct DefaultTextStyle {
    pub color: Option<StyleFn>,
    /// Background of whole lines, applied after padding.
    pub bg_color: Option<StyleFn>,
    pub bold: bool,
    pub italic: bool,
    pub strikethrough: bool,
    pub underline: bool,
}

/// Options of [`Markdown`].
#[derive(Clone)]
pub struct MarkdownOptions {
    /// Keep source list markers instead of normalizing them.
    pub preserve_ordered_list_markers: bool,
    /// Keep source backslash escapes instead of normalizing escaped punctuation.
    pub preserve_backslash_escapes: bool,
    /// Transforms the source before parsing, for extension Markdown transformers.
    pub transform: Option<MarkdownTransformFn>,
    /// Render supported LaTeX math as Unicode text (default true).
    pub render_latex: bool,
}

impl Default for MarkdownOptions {
    fn default() -> Self {
        Self {
            preserve_ordered_list_markers: false,
            preserve_backslash_escapes: false,
            transform: None,
            render_latex: true,
        }
    }
}

/// Markdown text with padding (upstream `Markdown`).
pub struct Markdown {
    text: String,
    padding_x: usize,
    padding_y: usize,
    default_text_style: Option<DefaultTextStyle>,
    theme: MarkdownTheme,
    options: MarkdownOptions,
    default_style_prefix: Option<String>,
    cache: Option<(String, usize, Vec<String>)>,
    /// Tokens depend only on the source, so they survive theme and width invalidation.
    cached_tokens: Option<(String, Rc<Vec<Token>>)>,
}

/// Trims a streamed partial closing fence so code blocks do not shrink when it completes.
fn trim_partial_closing_fences(tokens: &mut [Token]) {
    match tokens.last_mut() {
        Some(Token::List(list)) => {
            if let Some(item) = list.items.last_mut() {
                trim_partial_closing_fences(&mut item.tokens);
            }
        }
        Some(Token::Blockquote { tokens, .. }) => trim_partial_closing_fences(tokens),
        Some(Token::Code { raw, text, .. }) => {
            let marker_char = match raw.chars().next() {
                Some(c @ ('`' | '~')) => c,
                _ => return,
            };
            let marker_len = raw.chars().take_while(|c| *c == marker_char).count();
            let last_line = raw.rsplit('\n').next().unwrap_or("");
            if marker_len < 3
                || last_line.is_empty()
                || last_line.len() >= marker_len
                || last_line.chars().any(|c| c != marker_char)
            {
                return;
            }
            let kept = text.len().saturating_sub(last_line.len());
            let kept = &text[..block::floor_boundary(text, kept)];
            *text = kept.strip_suffix('\n').unwrap_or(kept).to_string();
        }
        _ => {}
    }
}

impl Markdown {
    pub fn new(
        text: &str,
        padding_x: usize,
        padding_y: usize,
        theme: MarkdownTheme,
        default_text_style: Option<DefaultTextStyle>,
        options: MarkdownOptions,
    ) -> Self {
        Self {
            text: text.to_string(),
            padding_x,
            padding_y,
            default_text_style,
            theme,
            options,
            default_style_prefix: None,
            cache: None,
            cached_tokens: None,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_string();
        self.cache = None;
    }

    fn tokens(&mut self, source: &str) -> Rc<Vec<Token>> {
        if let Some((cached_source, tokens)) = &self.cached_tokens
            && cached_source == source
        {
            return tokens.clone();
        }
        let mut tokens = lexer::lex(source);
        trim_partial_closing_fences(&mut tokens);
        let tokens = Rc::new(tokens);
        self.cached_tokens = Some((source.to_string(), tokens.clone()));
        tokens
    }

    fn pad_line(&self, line: &str, width: usize) -> String {
        match self.default_text_style.as_ref().and_then(|style| style.bg_color.as_ref()) {
            Some(bg) => apply_background_to_line(line, width, bg.as_ref()),
            None => format!("{line}{}", " ".repeat(width.saturating_sub(visible_width(line)))),
        }
    }
}

impl Component for Markdown {
    fn render(&mut self, width: usize) -> Vec<String> {
        if let Some((text, cached_width, lines)) = &self.cache
            && *text == self.text
            && *cached_width == width
        {
            return lines.clone();
        }

        let content_width = width.saturating_sub(self.padding_x * 2).max(1);
        let text = match &self.options.transform {
            Some(transform) => transform(&self.text, content_width),
            None => self.text.clone(),
        };
        if js_trim(&text).is_empty() {
            self.cache = Some((self.text.clone(), width, Vec::new()));
            return Vec::new();
        }

        let normalized = text.replace('\t', "   ");
        let tokens = self.tokens(&normalized);
        let mut rendered_lines = Vec::new();
        for (index, token) in tokens.iter().enumerate() {
            let next = tokens.get(index + 1).map(Token::kind);
            rendered_lines.extend(self.render_token(token, content_width, next, None));
        }

        let mut wrapped_lines = Vec::new();
        for line in rendered_lines {
            if is_image_line(&line) {
                wrapped_lines.push(line);
            } else {
                wrapped_lines.extend(wrap_text_with_ansi(&line, content_width));
            }
        }

        let margin = " ".repeat(self.padding_x);
        let content_lines: Vec<String> = wrapped_lines
            .into_iter()
            .map(
                |line| {
                    if is_image_line(&line) { line } else { self.pad_line(&format!("{margin}{line}{margin}"), width) }
                },
            )
            .collect();
        let empty_line = self.pad_line(&" ".repeat(width), width);
        let mut result = vec![empty_line.clone(); self.padding_y];
        result.extend(content_lines);
        result.extend(std::iter::repeat_n(empty_line, self.padding_y));
        flatten_lines(&mut result);
        self.cache = Some((self.text.clone(), width, result.clone()));
        if result.is_empty() { vec![String::new()] } else { result }
    }

    fn invalidate(&mut self) {
        self.cache = None;
    }
}
