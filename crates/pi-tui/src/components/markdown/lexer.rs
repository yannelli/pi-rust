//! A port of the `marked` 18 lexer with GFM rules, the upstream strict strikethrough tokenizer,
//! and the LaTeX extensions. Positions are UTF-8 byte offsets.

use std::collections::HashMap;

use super::latex_ext;
use super::token::{Children, Token, children};

/// A link reference definition (`[label]: href "title"`).
#[derive(Debug, Clone)]
pub(crate) struct LinkDef {
    pub href: String,
}

pub(crate) struct InlineJob {
    pub src: String,
    pub tokens: Children,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct LexerState {
    pub in_link: bool,
    pub in_raw_block: bool,
    pub link_emitted: bool,
    pub top: bool,
}

pub(crate) struct Lexer {
    pub links: HashMap<String, LinkDef>,
    pub queue: Vec<InlineJob>,
    pub state: LexerState,
}

/// Lexes Markdown into block tokens with their inline children.
pub(crate) fn lex(source: &str) -> Vec<Token> {
    let normalized = source.replace("\r\n", "\n").replace('\r', "\n");
    let mut lexer = Lexer {
        links: HashMap::new(),
        queue: Vec::new(),
        state: LexerState { in_link: false, in_raw_block: false, link_emitted: false, top: true },
    };
    let mut tokens = Vec::new();
    lexer.block_tokens(&normalized, &mut tokens, false);
    let queue = std::mem::take(&mut lexer.queue);
    for job in &queue {
        let mut out = job.tokens.borrow_mut();
        lexer.inline_tokens(&job.src, &mut out);
    }
    tokens
}

/// Byte offset of the second character (JavaScript `slice(1)`).
pub(crate) fn after_first_char(text: &str) -> usize {
    text.chars().next().map_or(0, char::len_utf8)
}

/// Removes `raw.len()` bytes from the front of `src`, staying on a character boundary.
pub(crate) fn advance<'a>(src: &'a str, raw: &str) -> &'a str {
    let mut end = raw.len().min(src.len());
    while !src.is_char_boundary(end) {
        end += 1;
    }
    &src[end..]
}

fn append_line(target: &mut String, addition: &str) {
    if !target.ends_with('\n') {
        target.push('\n');
    }
    target.push_str(addition);
}

impl Lexer {
    /// Queues inline lexing of `src` (marked's `inline`), returning the list it will fill.
    pub(crate) fn inline(&mut self, src: &str) -> Children {
        let tokens = children(Vec::new());
        self.queue.push(InlineJob { src: src.to_string(), tokens: tokens.clone() });
        tokens
    }

    fn set_last_queue_src(&mut self, src: &str) {
        if let Some(job) = self.queue.last_mut() {
            job.src = src.to_string();
        }
    }

    /// Merges block text into the preceding paragraph or text token, like marked does for
    /// indented code and definitions that cannot interrupt a paragraph.
    fn merge_into_last(&mut self, tokens: &mut [Token], raw: &str, text: &str) -> bool {
        let Some(last) = tokens.last_mut().filter(|t| matches!(t, Token::Paragraph { .. } | Token::Text { .. })) else {
            return false;
        };
        append_line(last.raw_mut(), raw);
        let merged = last.text_mut().map(|existing| {
            existing.push('\n');
            existing.push_str(text);
            existing.clone()
        });
        if let Some(merged) = merged {
            self.set_last_queue_src(&merged);
        }
        true
    }

    pub(crate) fn block_tokens(&mut self, source: &str, tokens: &mut Vec<Token>, mut last_paragraph_clipped: bool) {
        let mut src = source;
        let mut shortest = usize::MAX;
        while !src.is_empty() {
            if src.len() >= shortest {
                break;
            }
            shortest = src.len();

            if let Some(token) = latex_ext::tokenize_block(src) {
                src = advance(src, token.raw());
                tokens.push(token);
                continue;
            }
            if let Some(token) = self.space(src) {
                src = advance(src, token.raw());
                match tokens.last_mut() {
                    Some(last) if token.raw().len() == 1 => last.raw_mut().push('\n'),
                    _ => tokens.push(token),
                }
                continue;
            }
            if let Some(token) = self.code(src) {
                src = advance(src, token.raw());
                if let Token::Code { raw, text, .. } = &token
                    && self.merge_into_last(tokens, raw, text)
                {
                    continue;
                }
                tokens.push(token);
                continue;
            }
            let simple = [Self::fences, Self::heading, Self::hr, Self::blockquote, Self::list, Self::html];
            if let Some(token) = simple.iter().find_map(|tokenize| tokenize(self, src)) {
                src = advance(src, token.raw());
                tokens.push(token);
                continue;
            }
            if let Some((token, tag, def)) = self.def(src) {
                src = advance(src, token.raw());
                let raw = token.raw().to_string();
                if !self.merge_into_last(tokens, &raw, &raw) && !self.links.contains_key(&tag) {
                    self.links.insert(tag, def);
                    tokens.push(token);
                }
                continue;
            }
            if let Some(token) = self.table(src).or_else(|| self.lheading(src)) {
                src = advance(src, token.raw());
                tokens.push(token);
                continue;
            }

            let first_len = after_first_char(src);
            let cut = match latex_ext::block_start(&src[first_len..], first_len == 4) {
                Some(start) => &src[..first_len + start],
                None => src,
            };
            if self.state.top
                && let Some(token) = self.paragraph(cut)
            {
                let raw_len = token.raw().len();
                let merged = match (last_paragraph_clipped, tokens.last_mut(), &token) {
                    (true, Some(last @ Token::Paragraph { .. }), Token::Paragraph { raw, text, .. }) => {
                        append_line(last.raw_mut(), raw);
                        let text_field = last.text_mut().expect("paragraph text");
                        text_field.push('\n');
                        text_field.push_str(text);
                        Some(text_field.clone())
                    }
                    _ => None,
                };
                match merged {
                    Some(text) => {
                        self.queue.pop();
                        self.set_last_queue_src(&text);
                    }
                    None => tokens.push(token),
                }
                last_paragraph_clipped = cut.len() != src.len();
                src = &src[raw_len.min(src.len())..];
                continue;
            }
            if let Some(token) = self.text(src) {
                src = advance(src, token.raw());
                let merged = match (tokens.last_mut(), &token) {
                    (Some(last @ Token::Text { .. }), Token::Text { raw, text, .. }) => {
                        append_line(last.raw_mut(), raw);
                        let text_field = last.text_mut().expect("text");
                        text_field.push('\n');
                        text_field.push_str(text);
                        Some(text_field.clone())
                    }
                    _ => None,
                };
                match merged {
                    Some(text) => {
                        self.queue.pop();
                        self.set_last_queue_src(&text);
                    }
                    None => tokens.push(token),
                }
                continue;
            }
            break;
        }
        self.state.top = true;
    }

    pub(crate) fn inline_tokens(&mut self, source: &str, tokens: &mut Vec<Token>) {
        let masked = self.mask_inline_source(source);
        let mut src = source;
        let mut shortest = usize::MAX;
        let mut keep_prev_char = false;
        let mut prev_char = String::new();
        while !src.is_empty() {
            if src.len() >= shortest {
                break;
            }
            shortest = src.len();
            if !keep_prev_char {
                prev_char.clear();
            }
            keep_prev_char = false;

            if let Some(token) = latex_ext::tokenize_inline(src) {
                src = advance(src, token.raw());
                tokens.push(token);
                continue;
            }
            let found = self
                .escape(src)
                .or_else(|| self.tag(src))
                .or_else(|| self.link(src))
                .map(|token| (token, false))
                .or_else(|| self.reflink(src).map(|token| (token, true)));
            if let Some((token, merge_text)) = found {
                src = advance(src, token.raw());
                push_inline(tokens, token, merge_text);
                continue;
            }
            let masked_tail = &masked[masked.len() - src.len()..];
            let found = self
                .em_strong(src, masked_tail, &prev_char)
                .or_else(|| self.codespan(src))
                .or_else(|| self.br(src))
                .or_else(|| self.del(src))
                .or_else(|| self.autolink(src))
                .or_else(|| if self.state.in_link { None } else { self.url(src) });
            if let Some(token) = found {
                src = advance(src, token.raw());
                tokens.push(token);
                continue;
            }

            let cut = match latex_ext::inline_start(&src[after_first_char(src)..]) {
                Some(start) => &src[..after_first_char(src) + start],
                None => src,
            };
            if let Some(token) = self.inline_text(cut) {
                src = advance(src, token.raw());
                if let Some(last) = token.raw().chars().next_back().filter(|c| *c != '_') {
                    // JavaScript keeps only the low surrogate of an astral character, which is
                    // neither alphanumeric, punctuation, nor whitespace.
                    prev_char = if last.len_utf8() == 4 { "\u{e000}".to_string() } else { last.to_string() };
                }
                keep_prev_char = true;
                push_inline(tokens, token, true);
                continue;
            }
            break;
        }
    }
}

/// Pushes an inline token, merging plain text into a preceding text token when `merge_text`.
fn push_inline(tokens: &mut Vec<Token>, token: Token, merge_text: bool) {
    if merge_text
        && let Token::Text { raw, text, tokens: None } = &token
        && let Some(Token::Text { raw: last_raw, text: last_text, .. }) = tokens.last_mut()
    {
        last_raw.push_str(raw);
        last_text.push_str(text);
        return;
    }
    tokens.push(token);
}
