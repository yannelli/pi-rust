//! The list tokenizer of the marked lexer, with GFM task items.

use regex::Regex;

use super::lexer::Lexer;
use super::rules::{self, indent_rules};
use super::token::{ListItem, ListToken, Token, children};
use crate::utils::{js_trim, js_trim_end};

/// marked's tab expansion of the first item line, starting at `column`.
fn expand_tabs(text: &str, column: usize) -> String {
    let mut column = column;
    let mut result = String::with_capacity(text.len());
    for c in text.chars() {
        if c == '\t' {
            let width = 4 - column % 4;
            result.push_str(&" ".repeat(width));
            column += width;
        } else {
            result.push(c);
            column += 1;
        }
    }
    result
}

/// Index of the first character that is not a space (JavaScript `search(/[^ ]/)`).
fn first_non_space(text: &str) -> Option<usize> {
    text.chars().position(|c| c != ' ')
}

/// JavaScript `slice(count)` on text whose characters are in the Basic Multilingual Plane.
fn skip_chars(text: &str, count: usize) -> &str {
    text.char_indices().nth(count).map_or("", |(index, _)| &text[index..])
}

/// JavaScript `slice(count)` in UTF-16 units. Splitting an astral character leaves a lone
/// surrogate in JavaScript; a private-use character stands in for it, as neither is a space.
fn skip_utf16_units(text: &str, count: usize) -> String {
    let mut units = 0;
    for (index, c) in text.char_indices() {
        if units >= count {
            return text[index..].to_string();
        }
        units += c.len_utf16();
        if units > count {
            return format!("\u{e000}{}", &text[index + c.len_utf8()..]);
        }
    }
    String::new()
}

fn first_line(text: &str) -> &str {
    text.split('\n').next().unwrap_or("")
}

/// Drops `line` and its newline from the front of `src` (JavaScript `substring(length + 1)`).
fn after_line<'a>(src: &'a str, line: &str) -> &'a str {
    src.get(line.len() + 1..).unwrap_or("")
}

fn replace_task(text: &str) -> String {
    rules::LIST_REPLACE_TASK.replace(text, "").into_owned()
}

impl Lexer {
    pub(crate) fn list(&mut self, source: &str) -> Option<Token> {
        let cap = rules::LIST.captures(source)?;
        let bullet = js_trim(&cap[1]);
        let ordered = bullet.len() > 1;
        let start = if ordered { bullet[..bullet.len() - 1].parse().ok() } else { None };
        let mut list = ListToken { raw: String::new(), ordered, start, loose: false, items: Vec::new() };
        let marker =
            if ordered { format!(r"[0-9]{{1,9}}\{}", &bullet[bullet.len() - 1..]) } else { format!(r"\{bullet}") };
        let item_regex = Regex::new(&format!(r"^( {{0,3}}{marker})((?:[\t ][^\n]*)?(?:\n|$))")).ok()?;
        let mut ends_with_blank_line = false;
        let mut src = source;

        while !src.is_empty() {
            let Some(item) = item_regex.captures(src) else { break };
            if rules::HR.is_match(src) {
                break;
            }
            let mut raw = item[0].to_string();
            src = &src[raw.len()..];
            let bullet_width = item[1].len();
            let mut line = expand_tabs(first_line(&item[2]), bullet_width);
            let mut next_line = first_line(src);
            let mut blank_line = js_trim(&line).is_empty();
            let mut item_contents = String::new();
            let indent = if blank_line {
                bullet_width + 1
            } else {
                let mut indent = first_non_space(&line).unwrap_or(0);
                if indent > 4 {
                    indent = 1;
                }
                item_contents = skip_chars(&line, indent).to_string();
                indent + bullet_width
            };

            let mut end_early = false;
            if blank_line && rules::BLANK_LINE.is_match(next_line) {
                raw.push_str(next_line);
                raw.push('\n');
                src = after_line(src, next_line);
                end_early = true;
            }
            if !end_early {
                let rules = indent_rules(indent);
                while !src.is_empty() {
                    let raw_line = first_line(src);
                    next_line = raw_line;
                    let without_tabs = next_line.replace('\t', "    ");
                    let interrupted = rules.fences_begin.is_match(next_line)
                        || rules.heading_begin.is_match(next_line)
                        || rules.html_begin.is_match(next_line)
                        || rules.blockquote_begin.is_match(next_line)
                        || rules.next_bullet.is_match(next_line)
                        || rules.hr.is_match(next_line);
                    if interrupted {
                        break;
                    }
                    if first_non_space(&without_tabs).is_some_and(|i| i >= indent) || js_trim(next_line).is_empty() {
                        item_contents.push('\n');
                        item_contents.push_str(skip_chars(&without_tabs, indent));
                    } else {
                        let line_indent = first_non_space(&line.replace('\t', "    ")).is_some_and(|i| i >= 4);
                        if blank_line
                            || line_indent
                            || rules.fences_begin.is_match(&line)
                            || rules.heading_begin.is_match(&line)
                            || rules.hr.is_match(&line)
                        {
                            break;
                        }
                        item_contents.push('\n');
                        item_contents.push_str(next_line);
                    }
                    blank_line = js_trim(next_line).is_empty();
                    raw.push_str(raw_line);
                    raw.push('\n');
                    src = after_line(src, raw_line);
                    line = skip_utf16_units(&without_tabs, indent);
                }
            }

            if !list.loose {
                if ends_with_blank_line {
                    list.loose = true;
                } else if rules::DOUBLE_BLANK_LINE.is_match(&raw) {
                    ends_with_blank_line = true;
                }
            }
            let task = rules::LIST_IS_TASK.is_match(&item_contents);
            list.raw.push_str(&raw);
            list.items.push(ListItem { raw, task, checked: None, text: item_contents, tokens: Vec::new() });
        }

        let last = list.items.last_mut()?;
        last.raw = js_trim_end(&last.raw).to_string();
        last.text = js_trim_end(&last.text).to_string();
        list.raw = js_trim_end(&list.raw).to_string();

        for index in 0..list.items.len() {
            self.state.top = false;
            let mut tokens = Vec::new();
            let text = list.items[index].text.clone();
            self.block_tokens(&text, &mut tokens, false);
            if !list.loose {
                list.loose = tokens.iter().any(|t| matches!(t, Token::Space { raw } if rules::ANY_LINE.is_match(raw)));
            }
            list.items[index].tokens = tokens;
        }

        for index in 0..list.items.len() {
            self.apply_task(&mut list, index);
        }

        if list.loose {
            for item in &mut list.items {
                for token in &mut item.tokens {
                    if let Token::Text { raw, text, tokens } = token {
                        let tokens = tokens.take().unwrap_or_else(|| children(Vec::new()));
                        *token = Token::Paragraph { raw: std::mem::take(raw), text: std::mem::take(text), tokens };
                    }
                }
            }
        }
        Some(Token::List(list))
    }

    /// Strips the `[ ]` or `[x]` marker of a task item and records its checkbox.
    fn apply_task(&mut self, list: &mut ListToken, index: usize) {
        let loose = list.loose;
        let item = &mut list.items[index];
        let first_is_text = matches!(item.tokens.first(), Some(Token::Text { .. } | Token::Paragraph { .. }));
        if !item.task || !first_is_text {
            item.task = false;
            return;
        }
        item.text = replace_task(&item.text);
        if let Some(first) = item.tokens.first_mut() {
            *first.raw_mut() = replace_task(first.raw());
            if let Some(text) = first.text_mut() {
                *text = replace_task(text);
            }
        }
        if let Some(job) = self.queue.iter_mut().rev().find(|job| rules::LIST_IS_TASK.is_match(&job.src)) {
            job.src = replace_task(&job.src);
        }
        let Some(found) = rules::LIST_TASK_CHECKBOX.find(&item.raw) else {
            return;
        };
        let checkbox_raw = format!("{} ", found.as_str());
        item.checked = Some(found.as_str() != "[ ]");
        let checkbox = Token::Checkbox { raw: checkbox_raw.clone() };
        if !loose {
            item.tokens.insert(0, checkbox);
            return;
        }
        let first_children = match item.tokens.first() {
            Some(Token::Paragraph { tokens, .. }) | Some(Token::Text { tokens: Some(tokens), .. }) => {
                Some(tokens.clone())
            }
            _ => None,
        };
        match first_children {
            Some(tokens) => {
                let first = item.tokens.first_mut().expect("first token");
                first.raw_mut().insert_str(0, &checkbox_raw);
                if let Some(text) = first.text_mut() {
                    text.insert_str(0, &checkbox_raw);
                }
                tokens.borrow_mut().insert(0, checkbox);
            }
            None => item.tokens.insert(
                0,
                Token::Paragraph { raw: checkbox_raw.clone(), text: checkbox_raw, tokens: children(vec![checkbox]) },
            ),
        }
    }
}
