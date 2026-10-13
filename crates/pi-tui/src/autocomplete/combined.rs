//! [`CombinedAutocompleteProvider`]: slash commands, their arguments, and file paths.

use std::rc::Rc;

use super::fd::fuzzy_file_suggestions;
use super::files::get_file_suggestions;
use super::paths::{extract_at_prefix, extract_path_prefix, parse_path_prefix};
use super::{
    AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions, CommandEntry, CompletionResult, SuggestionOptions,
    SuggestionsFuture,
};
use crate::fuzzy::fuzzy_filter;
use crate::utils::{is_js_whitespace, js_trim};

const SKILL_PREFIX: &str = "skill:";

/// Completes slash commands at the start of a line, their arguments, `@` attachments (through
/// `fd`), and plain paths.
#[derive(Clone)]
pub struct CombinedAutocompleteProvider {
    commands: Rc<Vec<CommandEntry>>,
    base_path: String,
    fd_path: Option<String>,
}

impl CombinedAutocompleteProvider {
    /// `base_path` resolves relative paths; `fd_path` enables `@` fuzzy search.
    pub fn new(commands: Vec<CommandEntry>, base_path: impl Into<String>, fd_path: Option<String>) -> Self {
        Self { commands: Rc::new(commands), base_path: base_path.into(), fd_path }
    }

    fn command_suggestions(&self, command_text: &str) -> Option<AutocompleteSuggestions> {
        struct Candidate {
            name: String,
            description: Option<String>,
        }
        let candidates: Vec<Candidate> = self
            .commands
            .iter()
            .map(|entry| {
                let (hint, description) = match entry {
                    CommandEntry::Command(command) => {
                        (command.argument_hint.as_deref(), command.description.as_deref())
                    }
                    CommandEntry::Item(item) => (None, item.description.as_deref()),
                };
                let description = description.unwrap_or("");
                let full = match hint.filter(|hint| !hint.is_empty()) {
                    Some(hint) if !description.is_empty() => format!("{hint} \u{2014} {description}"),
                    Some(hint) => hint.to_string(),
                    None => description.to_string(),
                };
                Candidate { name: entry.name().to_string(), description: (!full.is_empty()).then_some(full) }
            })
            .collect();

        let query = &command_text[1..];
        let bare = fuzzy_filter(candidates.iter().collect(), query, |c: &&Candidate| {
            c.name.strip_prefix(SKILL_PREFIX).unwrap_or(&c.name)
        });
        let full_only: Vec<&Candidate> = candidates
            .iter()
            .filter(|c| c.name.starts_with(SKILL_PREFIX) && !bare.iter().any(|b| std::ptr::eq(*b, *c)))
            .collect();
        let full = fuzzy_filter(full_only, query, |c: &&Candidate| c.name.as_str());
        let items: Vec<AutocompleteItem> = bare
            .into_iter()
            .chain(full)
            .map(|c| AutocompleteItem {
                value: c.name.clone(),
                label: c.name.clone(),
                description: c.description.clone(),
            })
            .collect();
        (!items.is_empty()).then(|| AutocompleteSuggestions { items, prefix: command_text.to_string() })
    }
}

fn ready(value: Option<AutocompleteSuggestions>) -> SuggestionsFuture {
    Box::pin(std::future::ready(value))
}

/// Largest char boundary at or before `index`.
fn floor_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn line_at(lines: &[String], index: usize) -> &str {
    lines.get(index).map_or("", String::as_str)
}

impl AutocompleteProvider for CombinedAutocompleteProvider {
    fn get_suggestions(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        options: SuggestionOptions,
    ) -> SuggestionsFuture {
        let current_line = line_at(lines, cursor_line);
        let text_before = &current_line[..floor_boundary(current_line, cursor_col)];

        if let Some(at_prefix) = extract_at_prefix(text_before) {
            let parsed = parse_path_prefix(&at_prefix);
            let search = fuzzy_file_suggestions(
                self.fd_path.clone(),
                self.base_path.clone(),
                parsed.raw.to_string(),
                parsed.is_quoted,
                options.signal.clone(),
            );
            return Box::pin(async move {
                let items = search.await;
                (!items.is_empty()).then_some(AutocompleteSuggestions { items, prefix: at_prefix })
            });
        }

        let command_text = text_before.trim_start_matches(is_js_whitespace);
        if !options.force && command_text.starts_with('/') {
            let Some(space) = command_text.find(' ') else {
                return ready(self.command_suggestions(command_text));
            };
            let name = &command_text[1..space];
            let argument = command_text[space + 1..].to_string();
            let completer = self.commands.iter().find(|entry| entry.name() == name).and_then(|entry| match entry {
                CommandEntry::Command(command) => command.get_argument_completions.clone(),
                CommandEntry::Item(_) => None,
            });
            let Some(completer) = completer else {
                return ready(None);
            };
            let pending = completer(&argument);
            return Box::pin(async move {
                let items = pending.await.filter(|items| !items.is_empty())?;
                Some(AutocompleteSuggestions { items, prefix: argument })
            });
        }

        let Some(path_prefix) = extract_path_prefix(text_before, options.force) else {
            return ready(None);
        };
        let items = get_file_suggestions(&self.base_path, &path_prefix);
        ready((!items.is_empty()).then_some(AutocompleteSuggestions { items, prefix: path_prefix }))
    }

    fn apply_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> CompletionResult {
        let current_line = line_at(lines, cursor_line);
        let cursor_col = floor_boundary(current_line, cursor_col);
        let before_prefix = &current_line[..floor_boundary(current_line, cursor_col.saturating_sub(prefix.len()))];
        let after_cursor = &current_line[cursor_col..];
        let is_quoted_prefix = prefix.starts_with('"') || prefix.starts_with("@\"");
        let after_cursor = if is_quoted_prefix && item.value.ends_with('"') && after_cursor.starts_with('"') {
            &after_cursor[1..]
        } else {
            after_cursor
        };
        let replace = |new_line: String, cursor: usize| {
            let mut new_lines = lines.to_vec();
            if cursor_line < new_lines.len() {
                new_lines[cursor_line] = new_line;
            }
            CompletionResult { lines: new_lines, cursor_line, cursor_col: cursor }
        };
        let is_directory = item.label.ends_with('/');
        // Directories keep the cursor before a closing quote so completion can continue inside.
        let value_offset =
            if is_directory && item.value.ends_with('"') { item.value.len() - 1 } else { item.value.len() };

        // A command name: `/` at the start of the line with no further path separator.
        if prefix.starts_with('/') && js_trim(before_prefix).is_empty() && !prefix[1..].contains('/') {
            let new_line = format!("{before_prefix}/{} {after_cursor}", item.value);
            return replace(new_line, before_prefix.len() + item.value.len() + 2);
        }

        // An attachment: no space after directories so completion can continue.
        if prefix.starts_with('@') {
            let suffix = if is_directory { "" } else { " " };
            let new_line = format!("{before_prefix}{}{suffix}{after_cursor}", item.value);
            return replace(new_line, before_prefix.len() + value_offset + suffix.len());
        }

        // Command arguments and plain paths replace the prefix with the value.
        let new_line = format!("{before_prefix}{}{after_cursor}", item.value);
        replace(new_line, before_prefix.len() + value_offset)
    }

    fn should_trigger_file_completion(&self, lines: &[String], cursor_line: usize, cursor_col: usize) -> bool {
        let current_line = line_at(lines, cursor_line);
        let text_before = js_trim(&current_line[..floor_boundary(current_line, cursor_col)]);
        !(text_before.starts_with('/') && !text_before.contains(' '))
    }
}
