//! Inline tokenizers of the marked lexer, with the upstream strict strikethrough rule.

use super::block::{floor_boundary, rtrim};
use super::lexer::Lexer;
use super::rules::{self, captures, captures_anywhere, is_match};
use super::token::Token;
use crate::utils::{CJK_PUNCTUATION_REGEX, js_trim};

/// marked's `findClosingBracket` for `()`: index of the unbalanced `)`, `None` when balanced,
/// and `Err` when an opening bracket is left open.
fn find_closing_bracket(text: &str) -> Result<Option<usize>, ()> {
    if !text.contains(')') {
        return Ok(None);
    }
    let mut level = 0i64;
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 1,
            b'(' => level += 1,
            b')' => {
                level -= 1;
                if level < 0 {
                    return Ok(Some(index));
                }
            }
            _ => {}
        }
        index += 1;
    }
    if level > 0 { Err(()) } else { Ok(None) }
}

fn unescape_punctuation(text: &str) -> String {
    rules::ANY_PUNCTUATION.replace_all(text, "$1").into_owned()
}

fn same_length_mask(text: &str, fill: char) -> String {
    std::iter::repeat_n(fill, text.len()).collect()
}

/// Whether the character before the unread source is punctuation or East Asian text. JavaScript
/// hands over a single UTF-16 unit as `prev_char`, so the real character is recovered from the
/// masked source: a variation selector defers to the character it follows.
fn previous_char_is_cjk(prev_char: &str, masked_prefix: &str) -> bool {
    if is_match(&rules::PUNCTUATION_CJK, prev_char) {
        return true;
    }
    if prev_char.is_empty() {
        return false;
    }
    let mut chars = masked_prefix.chars().rev();
    let Some(mut last) = chars.next() else { return false };
    if ('\u{fe00}'..='\u{fe0e}').contains(&last)
        && let Some(base) = chars.next()
    {
        last = base;
    }
    ('\u{e0100}'..='\u{e01ef}').contains(&last) || is_cjk(last)
}

fn is_cjk(c: char) -> bool {
    rules::CJK_CHAR.is_match(c.encode_utf8(&mut [0; 4]))
}

/// Whether the character before a right delimiter match or the one after it is East Asian.
fn cjk_touches_delimiter(matched: &str, clipped: &str, end: usize) -> bool {
    matched.chars().next().is_some_and(is_cjk)
        || clipped.get(end..).and_then(|rest| rest.chars().next()).is_some_and(is_cjk)
}

fn text_token(text: &str) -> Token {
    Token::Text { raw: text.to_string(), text: text.to_string(), tokens: None }
}

impl Lexer {
    /// Masks links, code spans, tags, and escapes so emphasis delimiters inside them are ignored.
    /// Every replacement keeps the byte length, so offsets stay aligned with the source.
    pub(crate) fn mask_inline_source(&self, source: &str) -> String {
        let mut masked = source.to_string();
        if !self.links.is_empty() && masked.contains('[') {
            masked = self.mask_reflinks(&masked);
        }
        masked = rules::ANY_PUNCTUATION
            .replace_all(&masked, |c: &regex::Captures| same_length_mask(&c[0], '+'))
            .into_owned();
        rules::BLOCK_SKIP
            .replace_all(&masked, |c: &fancy_regex::Captures<'_, str>| {
                format!("[{}]", "a".repeat(c[0].len().saturating_sub(2)))
            })
            .into_owned()
    }

    fn mask_reflinks(&self, text: &str) -> String {
        rules::REFLINK_SEARCH
            .replace_all(text, |c: &fancy_regex::Captures<'_, str>| {
                let found = &c[0];
                let bracket = found.rfind('[').unwrap_or(0);
                let label = &found[bracket + 1..found.len() - 1];
                if !self.links.contains_key(label) {
                    return found.to_string();
                }
                if bracket > 1 && !found.starts_with('!') {
                    let inner = &found[1..bracket - 1];
                    if self.link_in_text(inner) {
                        let masked_inner = self.mask_reflinks(inner);
                        return format!("[{masked_inner}][{}]", "a".repeat(found.len() - bracket - 2));
                    }
                }
                format!("[{}]", "a".repeat(found.len() - 2))
            })
            .into_owned()
    }

    fn link_in_text(&self, text: &str) -> bool {
        if !text.contains('[') {
            return false;
        }
        for found in rules::BLOCK_SKIP.find_iter(text).flatten() {
            let preceded_by_bang = text[..found.start()].ends_with('!');
            if is_match(&rules::LINK, found.as_str()) && !preceded_by_bang {
                return true;
            }
        }
        for found in rules::REFLINK_SEARCH.find_iter(text).flatten() {
            let found = found.as_str();
            let bracket = found.rfind('[').unwrap_or(0);
            let defined = self.links.contains_key(&found[bracket + 1..found.len() - 1]);
            if !(found.starts_with('!') || !defined) && !(bracket > 1 && self.link_in_text(&found[1..bracket - 1])) {
                return true;
            }
        }
        false
    }

    pub(crate) fn escape(&mut self, src: &str) -> Option<Token> {
        let cap = rules::ESCAPE.captures(src)?;
        Some(Token::Escape { raw: cap[0].to_string(), text: cap[1].to_string() })
    }

    pub(crate) fn tag(&mut self, src: &str) -> Option<Token> {
        let raw = rules::TAG.find(src)?.as_str().to_string();
        let state = &mut self.state;
        if !state.in_link && rules::START_A_TAG.is_match(&raw) {
            state.in_link = true;
        } else if state.in_link && rules::END_A_TAG.is_match(&raw) {
            state.in_link = false;
        }
        if !state.in_raw_block && rules::START_PRE_SCRIPT_TAG.is_match(&raw) {
            state.in_raw_block = true;
        } else if state.in_raw_block && rules::END_PRE_SCRIPT_TAG.is_match(&raw) {
            state.in_raw_block = false;
        }
        Some(Token::Html { raw })
    }

    pub(crate) fn link(&mut self, src: &str) -> Option<Token> {
        let cap = captures(&rules::LINK, src)?;
        let mut raw = cap[0].to_string();
        let label = cap[1].to_string();
        let mut href = cap.get(2).map_or("", |m| m.as_str()).to_string();
        let trimmed = js_trim(&href).to_string();
        if trimmed.starts_with('<') {
            if !trimmed.ends_with('>') {
                return None;
            }
            let without_close = &trimmed[..trimmed.len() - 1];
            // marked counts the closing `>` too: an odd number of backslashes escapes it.
            if (without_close.len() - rtrim(without_close, '\\').len()) % 2 == 1 {
                return None;
            }
        } else {
            match find_closing_bracket(&href) {
                Err(()) => return None,
                Ok(Some(index)) => {
                    let length = if raw.starts_with('!') { 5 } else { 4 } + label.len() + index;
                    href.truncate(index);
                    raw = js_trim(&raw[..floor_boundary(&raw, length)]).to_string();
                }
                Ok(None) => {}
            }
        }
        let mut href = js_trim(&href).to_string();
        if href.starts_with('<') {
            href = href.get(1..href.len().saturating_sub(1)).unwrap_or("").to_string();
        }
        self.output_link(&raw, &label, unescape_punctuation(&href))
    }

    /// marked's `outputLink`: builds a link or image token and tracks nested links.
    fn output_link(&mut self, raw: &str, label: &str, href: String) -> Option<Token> {
        let text = rules::OUTPUT_LINK_REPLACE.replace_all(label, "$1").into_owned();
        let image = raw.starts_with('!');
        self.state.in_link = true;
        let link_emitted = self.state.link_emitted;
        let in_raw_block = self.state.in_raw_block;
        self.state.link_emitted = false;
        let mut tokens = Vec::new();
        self.inline_tokens(&text, &mut tokens);
        let nested_link = self.state.link_emitted;
        self.state.link_emitted = link_emitted;
        self.state.in_link = false;
        if !image {
            if nested_link {
                self.state.in_raw_block = in_raw_block;
                return None;
            }
            self.state.link_emitted = true;
        }
        Some(Token::Link { raw: raw.to_string(), href, text, tokens, image })
    }

    pub(crate) fn reflink(&mut self, src: &str) -> Option<Token> {
        let cap = captures(&rules::REFLINK, src).or_else(|| captures(&rules::NOLINK, src))?;
        let label = cap.get(2).filter(|m| !m.as_str().is_empty()).unwrap_or_else(|| cap.get(1).expect("label"));
        let key = rules::MULTIPLE_SPACE.replace_all(label.as_str(), " ").to_lowercase();
        let raw = cap[0].to_string();
        let Some(def) = self.links.get(&key).cloned() else {
            let first: String = raw.chars().take(1).collect();
            return Some(text_token(&first));
        };
        self.output_link(&raw, &cap[1], def.href)
    }

    /// marked's `emStrong` with the `marked-cjk-friendly` flanking rules, which treat East Asian
    /// characters like punctuation so `**这是测试。**后面` still closes. `masked` is the whole masked
    /// source and `src` its unread tail.
    pub(crate) fn em_strong(&mut self, src: &str, masked: &str, prev_char: &str) -> Option<Token> {
        let cap = captures(&rules::EM_STRONG_LDELIM, src)?;
        let group = |i: usize| cap.get(i).is_some_and(|m| !m.as_str().is_empty());
        if !(1..=4).any(group) {
            return None;
        }
        if group(4) && rules::UNICODE_ALPHANUMERIC.is_match(prev_char) {
            return None;
        }
        let masked_tail = &masked[masked.len() - src.len()..];
        let next_char = cap.get(1).or_else(|| cap.get(3)).map_or("", |m| m.as_str());
        let flanks = next_char.is_empty()
            || prev_char.is_empty()
            || is_match(&rules::PUNCTUATION, prev_char)
            || previous_char_is_cjk(prev_char, &masked[..masked.len() - src.len()])
            || rules::CJK_CHAR.is_match(next_char);
        if !flanks {
            return None;
        }
        let left_length = cap[0].chars().count() - 1;
        let delimiter = &cap[0][..1];
        let same_as_prev = prev_char == delimiter;
        let end_rule =
            if delimiter == "*" { &*rules::EM_STRONG_RDELIM_AST_CJK } else { &*rules::EM_STRONG_RDELIM_UND_CJK };
        let clipped = masked_tail.get(left_length..)?;
        let mut delim_total = left_length as i64;
        let mut mid_delim_total = 0i64;
        for found in end_rule.captures_iter(clipped) {
            let Ok(found) = found else { break };
            let Some(right) = (1..=6).filter_map(|i| found.get(i)).find(|m| !m.as_str().is_empty()) else {
                continue;
            };
            let mut right_length = right.as_str().chars().count() as i64;
            let is_group = |i: usize| found.get(i).is_some_and(|m| !m.as_str().is_empty());
            let whole = found.get(0).expect("match");
            let cjk_adjacent = (1..=4).any(is_group) && cjk_touches_delimiter(whole.as_str(), clipped, whole.end());
            let left_only = is_group(3) || is_group(4);
            let both = is_group(5) || is_group(6);
            if left_only && !cjk_adjacent {
                delim_total += right_length;
                continue;
            }
            if both || cjk_adjacent {
                if left_length % 3 != 0 && (left_length as i64 + right_length) % 3 == 0 {
                    mid_delim_total += right_length;
                    continue;
                }
                if same_as_prev && both {
                    break;
                }
            }
            delim_total -= right_length;
            if delim_total > 0 {
                continue;
            }
            right_length = right_length.min(right_length + delim_total + mid_delim_total);
            let first_char_len = whole.as_str().chars().next().map_or(0, char::len_utf8);
            let end = left_length + whole.start() + first_char_len + right_length.max(0) as usize;
            let raw = src.get(..end)?;
            if left_length.min(right_length.max(0) as usize) % 2 == 1 {
                let inner = if raw.len() >= 2 { &raw[1..raw.len() - 1] } else { "" };
                let mut tokens = Vec::new();
                self.inline_tokens(inner, &mut tokens);
                return Some(Token::Em { raw: raw.to_string(), tokens });
            }
            let inner = if raw.len() >= 4 { &raw[2..raw.len() - 2] } else { "" };
            let mut tokens = Vec::new();
            self.inline_tokens(inner, &mut tokens);
            return Some(Token::Strong { raw: raw.to_string(), tokens });
        }
        None
    }

    pub(crate) fn codespan(&mut self, src: &str) -> Option<Token> {
        let cap = captures(&rules::CODESPAN, src)?;
        let mut text = cap[2].replace('\n', " ");
        let has_non_space = text.contains(|c: char| c != ' ');
        if has_non_space && text.starts_with(' ') && text.ends_with(' ') && text.len() >= 2 {
            text = text[1..text.len() - 1].to_string();
        }
        Some(Token::Codespan { raw: cap[0].to_string(), text })
    }

    pub(crate) fn br(&mut self, src: &str) -> Option<Token> {
        let cap = captures(&rules::BR, src)?;
        Some(Token::Br { raw: cap[0].to_string() })
    }

    /// The upstream `StrictStrikethroughTokenizer`: only `~~text~~` is struck through.
    pub(crate) fn del(&mut self, src: &str) -> Option<Token> {
        let cap = captures(&rules::STRICT_DEL, src)?;
        let raw = cap[0].to_string();
        let mut tokens = Vec::new();
        self.inline_tokens(&cap[2], &mut tokens);
        Some(Token::Del { raw, tokens })
    }

    pub(crate) fn autolink(&mut self, src: &str) -> Option<Token> {
        let cap = captures(&rules::AUTOLINK, src)?;
        let text = cap[1].to_string();
        let href = if cap.get(2).is_some() { format!("mailto:{text}") } else { text.clone() };
        let tokens = vec![text_token(&text)];
        Some(Token::Link { raw: cap[0].to_string(), href, text, tokens, image: false })
    }

    pub(crate) fn url(&mut self, src: &str) -> Option<Token> {
        let cap = captures(&rules::URL, src)?;
        let (raw, href): (String, String) = if cap.get(2).is_some_and(|m| m.as_str() == "@") {
            (cap[0].to_string(), format!("mailto:{}", &cap[0]))
        } else {
            let mut current = cap[0].to_string();
            loop {
                let previous = current.clone();
                current = captures_anywhere(&rules::BACKPEDAL, &current).map_or(String::new(), |c| c[0].to_string());
                if current == previous {
                    break;
                }
            }
            let href = if cap.get(1).is_some_and(|m| m.as_str() == "www.") {
                format!("http://{current}")
            } else {
                current.clone()
            };
            (current, href)
        };
        if let Some(punctuation) = CJK_PUNCTUATION_REGEX.find(&raw) {
            return self.url(&raw[..punctuation.start()]);
        }
        let tokens = vec![text_token(&raw)];
        Some(Token::Link { raw: raw.clone(), href, text: raw, tokens, image: false })
    }

    pub(crate) fn inline_text(&mut self, src: &str) -> Option<Token> {
        let cap = captures(&rules::INLINE_TEXT, src)?;
        Some(text_token(&cap[0]))
    }
}
