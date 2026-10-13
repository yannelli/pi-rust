//! Block tokenizers of the marked lexer (all but lists).

use super::lexer::{Lexer, LinkDef};
use super::rules::{self, captures};
use super::token::{TableCell, TableToken, Token};
use crate::utils::{is_js_whitespace, js_trim};

/// marked's `rtrim(str, c)`: removes trailing `c` characters.
pub(crate) fn rtrim(text: &str, c: char) -> &str {
    text.trim_end_matches(c)
}

/// marked's `trimTrailingBlankLines`: drops trailing blank lines when there is more than one.
fn trim_trailing_blank_lines(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut last = lines.len() as i64 - 1;
    while last >= 0 && rules::BLANK_LINE.is_match(lines[last as usize]) {
        last -= 1;
    }
    if lines.len() as i64 - last <= 2 { text.to_string() } else { lines[..(last + 1) as usize].join("\n") }
}

/// marked's `indentCodeCompensation`: removes the fence indentation from the code lines.
fn indent_code_compensation(raw: &str, text: &str) -> String {
    let Some(indent) = rules::INDENT_CODE_COMPENSATION.captures(raw).map(|c| c[1].chars().count()) else {
        return text.to_string();
    };
    text.split('\n')
        .map(|line| {
            let leading = line.chars().take_while(|c| is_js_whitespace(*c)).count();
            if leading == 0 || leading < indent {
                return line.to_string();
            }
            line.chars().skip(indent).collect()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// marked's `splitCells`: splits a table row on unescaped pipes.
pub(crate) fn split_cells(row: &str, count: Option<usize>) -> Vec<String> {
    let mut spaced = String::with_capacity(row.len() + 8);
    let mut backslashes = 0;
    for c in row.chars() {
        if c == '|' {
            spaced.push_str(if backslashes % 2 == 1 { "|" } else { " |" });
        } else {
            spaced.push(c);
        }
        backslashes = if c == '\\' { backslashes + 1 } else { 0 };
    }
    let mut cells: Vec<String> = spaced.split(" |").map(str::to_string).collect();
    if cells.first().is_some_and(|first| js_trim(first).is_empty()) {
        cells.remove(0);
    }
    if cells.last().is_some_and(|last| js_trim(last).is_empty()) {
        cells.pop();
    }
    if let Some(count) = count {
        cells.resize(count, String::new());
    }
    cells.iter().map(|cell| js_trim(cell).replace("\\|", "|")).collect()
}

fn unescape_punctuation(text: &str) -> String {
    rules::ANY_PUNCTUATION.replace_all(text, "$1").into_owned()
}

impl Lexer {
    pub(crate) fn space(&mut self, src: &str) -> Option<Token> {
        let found = rules::NEWLINE.find(src)?;
        (!found.is_empty()).then(|| Token::Space { raw: found.as_str().to_string() })
    }

    pub(crate) fn code(&mut self, src: &str) -> Option<Token> {
        let found = rules::CODE.find(src)?;
        let raw = trim_trailing_blank_lines(found.as_str());
        let text = rules::CODE_REMOVE_INDENT.replace_all(&raw, "").into_owned();
        Some(Token::Code { raw, lang: String::new(), text })
    }

    pub(crate) fn fences(&mut self, src: &str) -> Option<Token> {
        let cap = captures(&rules::FENCES, src)?;
        let raw = cap[0].to_string();
        let text = indent_code_compensation(&raw, cap.get(3).map_or("", |m| m.as_str()));
        let lang = unescape_punctuation(js_trim(&cap[2]));
        Some(Token::Code { raw, lang, text })
    }

    pub(crate) fn heading(&mut self, src: &str) -> Option<Token> {
        let cap = captures(&rules::HEADING, src)?;
        let mut text = js_trim(&cap[2]).to_string();
        if text.ends_with('#') {
            let trimmed = rtrim(&text, '#');
            if trimmed.is_empty() || trimmed.ends_with(' ') {
                text = js_trim(trimmed).to_string();
            }
        }
        let tokens = self.inline(&text);
        Some(Token::Heading { raw: rtrim(&cap[0], '\n').to_string(), depth: cap[1].len(), tokens })
    }

    pub(crate) fn hr(&mut self, src: &str) -> Option<Token> {
        let found = rules::HR.find(src)?;
        Some(Token::Hr { raw: rtrim(found.as_str(), '\n').to_string() })
    }

    pub(crate) fn html(&mut self, src: &str) -> Option<Token> {
        let cap = captures(&rules::HTML, src)?;
        Some(Token::Html { raw: trim_trailing_blank_lines(&cap[0]) })
    }

    /// A definition token with its normalized label and target.
    pub(crate) fn def(&mut self, src: &str) -> Option<(Token, String, LinkDef)> {
        let cap = captures(&rules::DEF, src)?;
        let tag = rules::MULTIPLE_SPACE.replace_all(&cap[1].to_lowercase(), " ").into_owned();
        let href =
            cap.get(2).map_or(String::new(), |m| unescape_punctuation(&rules::HREF_BRACKETS.replace(m.as_str(), "$1")));
        let raw = rtrim(&cap[0], '\n').to_string();
        Some((Token::Def { raw }, tag, LinkDef { href }))
    }

    pub(crate) fn table(&mut self, src: &str) -> Option<Token> {
        let cap = captures(&rules::TABLE, src)?;
        let delimiter = cap.get(2).map_or("", |m| m.as_str());
        if !delimiter.contains([':', '|']) {
            return None;
        }
        let header = split_cells(&cap[1], None);
        let aligns: Vec<String> =
            rules::TABLE_ALIGN_CHARS.replace_all(delimiter, "").split('|').map(str::to_string).collect();
        let body = cap.get(3).map_or("", |m| m.as_str());
        let rows: Vec<String> = if js_trim(body).is_empty() {
            Vec::new()
        } else {
            rules::TABLE_ROW_BLANK_LINE.replace(body, "").split('\n').map(str::to_string).collect()
        };
        if header.len() != aligns.len() {
            return None;
        }
        let header_cells = header.iter().map(|cell| TableCell { tokens: self.inline(cell) }).collect();
        let row_cells = rows
            .iter()
            .map(|row| {
                split_cells(row, Some(header.len()))
                    .iter()
                    .map(|cell| TableCell { tokens: self.inline(cell) })
                    .collect()
            })
            .collect();
        let raw = rtrim(&cap[0], '\n').to_string();
        Some(Token::Table(TableToken { raw, header: header_cells, rows: row_cells }))
    }

    pub(crate) fn lheading(&mut self, src: &str) -> Option<Token> {
        let cap = captures(&rules::LHEADING, src)?;
        let text = js_trim(&cap[1]).to_string();
        let depth = if cap[2].starts_with('=') { 1 } else { 2 };
        let tokens = self.inline(&text);
        Some(Token::Heading { raw: rtrim(&cap[0], '\n').to_string(), depth, tokens })
    }

    pub(crate) fn paragraph(&mut self, src: &str) -> Option<Token> {
        let cap = captures(&rules::PARAGRAPH, src)?;
        let content = &cap[1];
        let text = content.strip_suffix('\n').unwrap_or(content).to_string();
        let tokens = self.inline(&text);
        Some(Token::Paragraph { raw: cap[0].to_string(), text, tokens })
    }

    pub(crate) fn text(&mut self, src: &str) -> Option<Token> {
        let found = rules::TEXT.find(src)?;
        let raw = found.as_str().to_string();
        let tokens = self.inline(&raw);
        Some(Token::Text { text: raw.clone(), raw, tokens: Some(tokens) })
    }

    pub(crate) fn blockquote(&mut self, src: &str) -> Option<Token> {
        let cap = captures(&rules::BLOCKQUOTE, src)?;
        let mut lines: Vec<String> = rtrim(&cap[0], '\n').split('\n').map(str::to_string).collect();
        let mut raw = String::new();
        let mut text = String::new();
        let mut tokens: Vec<Token> = Vec::new();
        while !lines.is_empty() {
            let mut in_blockquote = false;
            let mut current = Vec::new();
            let mut index = 0;
            while index < lines.len() {
                if rules::BLOCKQUOTE_START.is_match(&lines[index]) {
                    current.push(lines[index].clone());
                    in_blockquote = true;
                } else if !in_blockquote {
                    current.push(lines[index].clone());
                } else {
                    break;
                }
                index += 1;
            }
            lines.drain(..index);
            let current_raw = current.join("\n");
            let setext = rules::BLOCKQUOTE_SETEXT.replace_all(&current_raw, "\n    $1");
            let current_text = rules::BLOCKQUOTE_PREFIX.replace_all(&setext, "").into_owned();
            raw = if raw.is_empty() { current_raw.clone() } else { format!("{raw}\n{current_raw}") };
            text = if text.is_empty() { current_text.clone() } else { format!("{text}\n{current_text}") };
            let top = self.state.top;
            self.state.top = true;
            self.block_tokens(&current_text, &mut tokens, true);
            self.state.top = top;
            if lines.is_empty() {
                break;
            }
            match tokens.last() {
                Some(Token::Code { .. }) => break,
                Some(Token::Blockquote { raw: old_raw, text: old_text, .. }) => {
                    let rest = lines.join("\n");
                    let joined = format!("{old_raw}\n{}", rules::BLOCKQUOTE_PREFIX.replace_all(&rest, ""));
                    let old_text_len = old_text.len();
                    if let Some(replacement @ Token::Blockquote { .. }) = self.blockquote(&joined) {
                        raw = format!("{raw}\n{rest}");
                        let kept = floor_boundary(&text, text.len().saturating_sub(old_text_len));
                        if let Token::Blockquote { text: new_text, .. } = &replacement {
                            text = format!("{}{new_text}", &text[..kept]);
                        }
                        *tokens.last_mut().expect("last token") = replacement;
                    }
                    break;
                }
                Some(Token::List(old)) => {
                    let old_raw_len = old.raw.len();
                    let joined = format!("{}\n{}", old.raw, lines.join("\n"));
                    let Some(replacement) = self.list(&joined) else {
                        break;
                    };
                    let new_raw = replacement.raw().to_string();
                    let raw_kept = floor_boundary(&raw, raw.len().saturating_sub(old_raw_len));
                    raw = format!("{}{new_raw}", &raw[..raw_kept]);
                    let text_kept = floor_boundary(&text, text.len().saturating_sub(old_raw_len));
                    text = format!("{}{new_raw}", &text[..text_kept]);
                    *tokens.last_mut().expect("last token") = replacement;
                    lines = joined.get(new_raw.len()..).unwrap_or("").split('\n').map(str::to_string).collect();
                }
                _ => {}
            }
        }
        Some(Token::Blockquote { raw, text, tokens })
    }
}

/// Largest character boundary of `text` at or before `index`.
pub(crate) fn floor_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}
