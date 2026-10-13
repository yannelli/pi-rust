//! Rendering of block and inline tokens to styled lines (upstream `renderToken` and friends).

use std::rc::Rc;
use std::sync::LazyLock;

use regex::Regex;

use super::token::{ListToken, Token};
use super::{DefaultTextStyle, Markdown, MarkdownTheme};
use crate::latex::{RenderLatexOptions, render_latex, render_latex_with};
use crate::terminal_image::{get_capabilities, hyperlink};
use crate::utils::{js_trim, visible_width, wrap_text_with_ansi};

const SENTINEL: &str = "\u{0}";

static ORDERED_MARKER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?: {0,3})([0-9]{1,9}[.)])[ \t]+").expect("regex"));

/// How inline text is styled, and the codes that restore that style after nested formatting.
#[derive(Clone)]
pub(super) struct StyleContext {
    apply_text: Rc<dyn Fn(&str) -> String>,
    pub(super) style_prefix: String,
}

fn apply_default_style(theme: &MarkdownTheme, style: Option<&DefaultTextStyle>, text: &str) -> String {
    let Some(style) = style else {
        return text.to_string();
    };
    let mut styled = text.to_string();
    if let Some(color) = &style.color {
        styled = color(&styled);
    }
    for (enabled, apply) in [
        (style.bold, &theme.bold),
        (style.italic, &theme.italic),
        (style.strikethrough, &theme.strikethrough),
        (style.underline, &theme.underline),
    ] {
        if enabled {
            styled = apply(&styled);
        }
    }
    styled
}

/// The codes a style function emits before its text.
fn style_prefix(style: &dyn Fn(&str) -> String) -> String {
    let styled = style(SENTINEL);
    styled.find(SENTINEL).map_or(String::new(), |index| styled[..index].to_string())
}

/// The source marker of an unordered list item (`- `, `+ `, or `* `).
fn unordered_marker(raw: &str) -> Option<String> {
    let rest = raw.trim_start_matches(' ');
    if raw.len() - rest.len() > 3 {
        return None;
    }
    let bullet = rest.chars().next().filter(|c| matches!(c, '-' | '+' | '*'))?;
    let after = &rest[1..];
    let valid =
        after.starts_with([' ', '\t']) || after.is_empty() || after.starts_with('\n') || after.starts_with("\r\n");
    valid.then(|| format!("{bullet} "))
}

impl Markdown {
    fn apply_default_style(&self, text: &str) -> String {
        apply_default_style(&self.theme, self.default_text_style.as_ref(), text)
    }

    fn default_context(&mut self) -> StyleContext {
        let prefix = match &self.default_style_prefix {
            Some(prefix) => prefix.clone(),
            None => {
                let prefix = if self.default_text_style.is_some() {
                    let styled = self.apply_default_style(SENTINEL);
                    styled.find(SENTINEL).map_or(String::new(), |index| styled[..index].to_string())
                } else {
                    String::new()
                };
                self.default_style_prefix = Some(prefix.clone());
                prefix
            }
        };
        let theme = self.theme.clone();
        let style = self.default_text_style.clone();
        StyleContext {
            apply_text: Rc::new(move |text| apply_default_style(&theme, style.as_ref(), text)),
            style_prefix: prefix,
        }
    }

    fn spacing(lines: &mut Vec<String>, next: Option<&str>) {
        if next.is_some_and(|next| next != "space") {
            lines.push(String::new());
        }
    }

    pub(super) fn render_token(
        &mut self,
        token: &Token,
        width: usize,
        next: Option<&str>,
        context: Option<&StyleContext>,
    ) -> Vec<String> {
        let mut lines = Vec::new();
        match token {
            Token::Heading { depth, tokens, .. } => {
                let theme = self.theme.clone();
                let style: Rc<dyn Fn(&str) -> String> = if *depth == 1 {
                    Rc::new(move |text| (theme.heading)(&(theme.bold)(&(theme.underline)(text))))
                } else {
                    Rc::new(move |text| (theme.heading)(&(theme.bold)(text)))
                };
                let heading_context =
                    StyleContext { style_prefix: style_prefix(style.as_ref()), apply_text: style.clone() };
                let text = self.render_inline_tokens(&tokens.borrow(), Some(&heading_context));
                if *depth >= 3 {
                    lines.push(format!("{}{text}", style(&format!("{} ", "#".repeat(*depth)))));
                } else {
                    lines.push(text);
                }
                Self::spacing(&mut lines, next);
            }
            Token::Paragraph { tokens, .. } => {
                lines.push(self.render_inline_tokens(&tokens.borrow(), context));
                if next.is_some_and(|next| next != "list" && next != "space") {
                    lines.push(String::new());
                }
            }
            Token::Text { .. } => lines.push(self.render_inline_tokens(std::slice::from_ref(token), context)),
            Token::LatexBlock { raw, text, pending } => {
                let source = js_trim(raw);
                let rendered = if !pending && self.options.render_latex {
                    render_latex_with(text, RenderLatexOptions { display: true }).unwrap_or_else(|| source.to_string())
                } else {
                    source.to_string()
                };
                lines.extend(rendered.split('\n').map(|line| self.apply_default_style(line)));
                Self::spacing(&mut lines, next);
            }
            Token::Code { lang, text, .. } => {
                let indent = self.theme.code_block_indent.clone().unwrap_or_else(|| "  ".into());
                lines.push((self.theme.code_block_border)(&format!("```{lang}")));
                match &self.theme.highlight_code {
                    Some(highlight) => {
                        let lang = (!lang.is_empty()).then_some(lang.as_str());
                        lines.extend(highlight(text, lang).iter().map(|line| format!("{indent}{line}")));
                    }
                    None => {
                        lines.extend(text.split('\n').map(|line| format!("{indent}{}", (self.theme.code_block)(line))));
                    }
                }
                lines.push((self.theme.code_block_border)("```"));
                Self::spacing(&mut lines, next);
            }
            Token::List(list) => lines.extend(self.render_list(list, 0, width, context)),
            Token::Table(table) => lines.extend(self.render_table(table, width, next, context)),
            Token::Blockquote { tokens, .. } => {
                lines.extend(self.render_blockquote(tokens, width));
                Self::spacing(&mut lines, next);
            }
            Token::Hr { .. } => {
                lines.push((self.theme.hr)(&"─".repeat(width.min(80))));
                Self::spacing(&mut lines, next);
            }
            Token::Html { raw } => lines.push(self.apply_default_style(js_trim(raw))),
            Token::Space { .. } => lines.push(String::new()),
            _ => {}
        }
        lines
    }

    fn render_blockquote(&mut self, tokens: &[Token], width: usize) -> Vec<String> {
        let theme = self.theme.clone();
        let quote_style: Rc<dyn Fn(&str) -> String> = Rc::new(move |text| (theme.quote)(&(theme.italic)(text)));
        let prefix = style_prefix(quote_style.as_ref());
        let content_width = width.saturating_sub(2).max(1);
        // The default message style does not apply inside block quotes.
        let context = StyleContext { apply_text: Rc::new(|text| text.to_string()), style_prefix: prefix.clone() };
        let mut rendered = Vec::new();
        for (index, token) in tokens.iter().enumerate() {
            let next = tokens.get(index + 1).map(Token::kind);
            rendered.extend(self.render_token(token, content_width, next, Some(&context)));
        }
        while rendered.last().is_some_and(String::is_empty) {
            rendered.pop();
        }
        let mut lines = Vec::new();
        for line in rendered {
            let styled = if prefix.is_empty() {
                quote_style(&line)
            } else {
                quote_style(&line.replace("\x1b[0m", &format!("\x1b[0m{prefix}")))
            };
            for wrapped in wrap_text_with_ansi(&styled, content_width) {
                lines.push(format!("{}{wrapped}", (self.theme.quote_border)("│ ")));
            }
        }
        lines
    }

    pub(super) fn render_inline_tokens(&mut self, tokens: &[Token], context: Option<&StyleContext>) -> String {
        let context = match context {
            Some(context) => context.clone(),
            None => self.default_context(),
        };
        let apply =
            |text: &str| text.split('\n').map(|segment| (context.apply_text)(segment)).collect::<Vec<_>>().join("\n");
        let prefix = context.style_prefix.as_str();
        let mut result = String::new();
        for token in tokens {
            match token {
                Token::Latex { raw, text, pending } => {
                    let rendered = if !pending && self.options.render_latex {
                        render_latex(text).unwrap_or_else(|| raw.clone())
                    } else {
                        raw.clone()
                    };
                    result.push_str(&apply(&rendered));
                }
                Token::Escape { raw, text } => {
                    result.push_str(&apply(if self.options.preserve_backslash_escapes { raw } else { text }));
                }
                Token::Text { text, tokens, .. } => {
                    match tokens.as_ref().filter(|tokens| !tokens.borrow().is_empty()) {
                        Some(children) => {
                            result.push_str(&self.render_inline_tokens(&children.borrow(), Some(&context)))
                        }
                        None => result.push_str(&apply(text)),
                    }
                }
                Token::Paragraph { tokens, .. } => {
                    result.push_str(&self.render_inline_tokens(&tokens.borrow(), Some(&context)));
                }
                Token::Strong { tokens, .. } => {
                    let content = self.render_inline_tokens(tokens, Some(&context));
                    result.push_str(&(self.theme.bold)(&content));
                    result.push_str(prefix);
                }
                Token::Em { tokens, .. } => {
                    let content = self.render_inline_tokens(tokens, Some(&context));
                    result.push_str(&(self.theme.italic)(&content));
                    result.push_str(prefix);
                }
                Token::Codespan { text, .. } => {
                    result.push_str(&(self.theme.code)(text));
                    result.push_str(prefix);
                }
                Token::Link { href, text, tokens, image: false, .. } => {
                    let link_text = self.render_inline_tokens(tokens, Some(&context));
                    let styled = (self.theme.link)(&(self.theme.underline)(&link_text));
                    if get_capabilities().hyperlinks {
                        // OSC 8 links show only the text; the URL is not printed.
                        result.push_str(&hyperlink(&styled, href));
                    } else {
                        let compared = href.strip_prefix("mailto:").unwrap_or(href);
                        result.push_str(&styled);
                        if text != href && text != compared {
                            result.push_str(&(self.theme.link_url)(&format!(" ({href})")));
                        }
                    }
                    result.push_str(prefix);
                }
                Token::Link { text, image: true, .. } => result.push_str(&apply(text)),
                Token::Br { .. } => result.push('\n'),
                Token::Del { tokens, .. } => {
                    let content = self.render_inline_tokens(tokens, Some(&context));
                    result.push_str(&(self.theme.strikethrough)(&content));
                    result.push_str(prefix);
                }
                Token::Html { raw } => result.push_str(&apply(raw)),
                _ => {}
            }
        }
        while !prefix.is_empty() && result.ends_with(prefix) {
            result.truncate(result.len() - prefix.len());
        }
        result
    }

    fn render_list(
        &mut self,
        list: &ListToken,
        depth: usize,
        width: usize,
        context: Option<&StyleContext>,
    ) -> Vec<String> {
        let mut lines = Vec::new();
        let indent = "    ".repeat(depth);
        let start = list.start.unwrap_or(1);
        let preserve = self.options.preserve_ordered_list_markers;
        for (index, item) in list.items.iter().enumerate() {
            let number = format!("{}. ", start + index as u64);
            let bullet = match (list.ordered, preserve) {
                (true, true) => ORDERED_MARKER.captures(&item.raw).map_or(number, |c| format!("{} ", &c[1])),
                (true, false) => number,
                (false, true) => unordered_marker(&item.raw).unwrap_or_else(|| "- ".into()),
                (false, false) => "- ".into(),
            };
            let task = if item.task {
                format!("[{}] ", if item.checked == Some(true) { "x" } else { " " })
            } else {
                String::new()
            };
            let marker = format!("{bullet}{task}");
            let first_prefix = format!("{indent}{}", (self.theme.list_bullet)(&marker));
            let continuation = format!("{indent}{}", " ".repeat(visible_width(&marker)));
            let item_width = width.saturating_sub(visible_width(&first_prefix)).max(1);
            let mut rendered_any = false;
            for token in &item.tokens {
                if let Token::List(nested) = token {
                    lines.extend(self.render_list(nested, depth + 1, width, context));
                    rendered_any = true;
                    continue;
                }
                for line in self.render_token(token, item_width, None, context) {
                    for wrapped in wrap_text_with_ansi(&line, item_width) {
                        let prefix = if rendered_any { &continuation } else { &first_prefix };
                        lines.push(format!("{prefix}{wrapped}"));
                        rendered_any = true;
                    }
                }
            }
            if !rendered_any {
                lines.push(first_prefix);
            }
            if list.loose && index + 1 < list.items.len() {
                lines.push(String::new());
            }
        }
        lines
    }
}
