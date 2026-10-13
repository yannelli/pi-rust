//! Text changes: typing, paste, newlines, submit, deletion, kill and yank.

use std::sync::LazyLock;

use regex::{Captures, Regex};

use super::EditorEvent;
use super::segment::{PASTE_MARKER_REGEX, paste_marker_id};
use super::state::{EditorCore, LastAction, TextState};
use crate::utils::{CJK_BREAK_REGEX, is_whitespace_char};

/// CSI-u Ctrl+letter sequences that tmux popups use for control bytes inside a paste.
static CSI_U_CTRL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[(\d+);5u").unwrap_or_else(|err| panic!("csi-u regex: {err}")));

static AUTOCOMPLETE_WORD_CHAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[a-zA-Z0-9.\-_]").unwrap_or_else(|err| panic!("word char regex: {err}")));

const LARGE_PASTE_LINES: usize = 10;
const LARGE_PASTE_CHARS: usize = 1000;

impl EditorCore {
    /// Inserts typed text. Word characters coalesce into one undo unit; each whitespace starts one.
    pub fn insert_character(&mut self, text: &str) {
        self.exit_history_browsing();
        if is_whitespace_char(text) || self.last_action != Some(LastAction::TypeWord) {
            self.push_undo_snapshot();
        }
        self.last_action = Some(LastAction::TypeWord);

        let col = self.state.cursor_col;
        let line = self.state.cursor_line;
        self.state.lines[line].insert_str(col, text);
        self.set_cursor_col(col + text.len());
        self.emit_change();

        if self.autocomplete_mode.is_some() {
            self.update_autocomplete();
            return;
        }
        if text == "/" && self.is_at_start_of_message() {
            self.try_trigger_autocomplete();
        } else if self.trigger_characters.iter().any(|c| c == text) {
            if self.matches_trigger_context(self.text_before_cursor()) {
                self.try_trigger_autocomplete();
            }
        } else if AUTOCOMPLETE_WORD_CHAR.is_match(text) || CJK_BREAK_REGEX.is_match(text) {
            self.retrigger_autocomplete_in_context();
        }
    }

    /// Requests completions when the cursor is in a slash command or a trigger token.
    pub fn retrigger_autocomplete_in_context(&mut self) {
        let before = self.text_before_cursor();
        if self.is_in_slash_command_context(before) || self.matches_trigger_context(before) {
            self.try_trigger_autocomplete();
        }
    }

    pub fn insert_text_at_cursor(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.cancel_autocomplete();
        self.push_undo_snapshot();
        self.last_action = None;
        self.exit_history_browsing();
        self.insert_text_at_cursor_internal(text);
    }

    /// Inserts possibly multi-line text without an undo snapshot or autocomplete.
    pub fn insert_text_at_cursor_internal(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let normalized = Self::normalize_text(text);
        let inserted: Vec<&str> = normalized.split('\n').collect();
        let line_index = self.state.cursor_line;
        let current = self.current_line().to_string();
        let (before, after) = current.split_at(self.state.cursor_col.min(current.len()));

        if inserted.len() == 1 {
            self.state.lines[line_index] = format!("{before}{normalized}{after}");
            self.set_cursor_col(before.len() + normalized.len());
        } else {
            let last = inserted[inserted.len() - 1];
            let mut replacement = vec![format!("{before}{}", inserted[0])];
            replacement.extend(inserted[1..inserted.len() - 1].iter().map(|s| s.to_string()));
            replacement.push(format!("{last}{after}"));
            self.state.lines.splice(line_index..=line_index, replacement);
            self.state.cursor_line += inserted.len() - 1;
            self.set_cursor_col(last.len());
        }
        self.emit_change();
    }

    pub fn handle_paste(&mut self, pasted: &str) {
        self.cancel_autocomplete();
        self.exit_history_browsing();
        self.last_action = None;
        self.push_undo_snapshot();

        let decoded = CSI_U_CTRL.replace_all(pasted, |caps: &Captures| {
            let code: u32 = caps[1].parse().unwrap_or(0);
            match code {
                97..=122 => char::from_u32(code - 96).map_or_else(|| caps[0].to_string(), String::from),
                65..=90 => char::from_u32(code - 64).map_or_else(|| caps[0].to_string(), String::from),
                _ => caps[0].to_string(),
            }
        });
        // Drop control characters except newlines.
        let mut filtered: String =
            Self::normalize_text(&decoded).chars().filter(|&c| c == '\n' || u32::from(c) >= 32).collect();

        // A pasted path right after a word character gets a separating space.
        if filtered.starts_with(['/', '~', '.']) {
            let before = self.text_before_cursor();
            if before.chars().next_back().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') {
                filtered.insert(0, ' ');
            }
        }

        let line_count = filtered.split('\n').count();
        let char_count = filtered.encode_utf16().count();
        if line_count > LARGE_PASTE_LINES || char_count > LARGE_PASTE_CHARS {
            self.paste_counter += 1;
            let id = self.paste_counter;
            self.pastes.set(id, filtered);
            let marker = if line_count > LARGE_PASTE_LINES {
                format!("[paste #{id} +{line_count} lines]")
            } else {
                format!("[paste #{id} {char_count} chars]")
            };
            self.insert_text_at_cursor_internal(&marker);
            return;
        }
        self.insert_text_at_cursor_internal(&filtered);
    }

    pub fn add_new_line(&mut self) {
        self.cancel_autocomplete();
        self.exit_history_browsing();
        self.last_action = None;
        self.push_undo_snapshot();

        let line_index = self.state.cursor_line;
        let after = self.state.lines[line_index].split_off(self.state.cursor_col);
        self.state.lines.insert(line_index + 1, after);
        self.state.cursor_line += 1;
        self.set_cursor_col(0);
        self.emit_change();
    }

    pub fn submit_value(&mut self) {
        self.cancel_autocomplete();
        let result = self.submission_text();
        self.state = TextState::default();
        self.pastes.clear();
        self.paste_counter = 0;
        self.exit_history_browsing();
        self.scroll_offset = 0;
        self.undo_stack.clear();
        self.last_action = None;
        self.events.push_back(EditorEvent::Change(String::new()));
        self.events.push_back(EditorEvent::Submit(result));
    }

    pub fn char_before_cursor_is_backslash(&self) -> bool {
        self.state.cursor_col > 0 && self.text_before_cursor().ends_with('\\')
    }

    pub fn handle_backspace(&mut self) {
        self.exit_history_browsing();
        self.last_action = None;

        if self.state.cursor_col > 0 {
            self.push_undo_snapshot();
            let before = self.text_before_cursor();
            let last = self.graphemes(before).last().map(|s| s.segment.to_string()).unwrap_or_default();
            let length = if last.is_empty() { 1 } else { last.len() };
            let col = self.state.cursor_col;
            let line = self.state.cursor_line;
            self.state.lines[line].replace_range(col - length..col, "");
            self.set_cursor_col(col - length);
            if let Some(target) = paste_marker_id(&last) {
                self.remove_paste(target);
            }
        } else if self.state.cursor_line > 0 {
            self.push_undo_snapshot();
            let line = self.state.cursor_line;
            let current = self.state.lines.remove(line);
            let joined_at = self.state.lines[line - 1].len();
            self.state.lines[line - 1].push_str(&current);
            self.state.cursor_line -= 1;
            self.set_cursor_col(joined_at);
        }
        self.emit_change();

        if self.autocomplete_mode.is_some() {
            self.update_autocomplete();
        } else {
            // Completion may have closed for lack of matches; deleting can bring matches back.
            self.retrigger_autocomplete_in_context();
        }
    }

    /// Forgets paste `target` and renumbers higher ids (and their markers) down by one.
    fn remove_paste(&mut self, target: u32) {
        self.pastes.remove(target);
        self.paste_counter = self.paste_counter.saturating_sub(1);
        let mut higher: Vec<u32> = self.pastes.ids().into_iter().filter(|id| *id > target).collect();
        higher.sort_unstable();
        for id in higher {
            if let Some(content) = self.pastes.get(id).map(str::to_string) {
                self.pastes.set(id - 1, content);
            }
            self.pastes.remove(id);
        }
        let renumber = |caps: &Captures| match caps[1].parse::<u32>() {
            Ok(id) if id > target => format!("[paste #{}{}]", id - 1, caps.get(2).map_or("", |m| m.as_str())),
            _ => caps[0].to_string(),
        };
        // Markers before the cursor can get shorter (`#10` becomes `#9`); keep the cursor after them.
        let col = self.state.cursor_col;
        let shift: usize = PASTE_MARKER_REGEX
            .captures_iter(self.current_line())
            .filter(|caps| caps.get(0).is_some_and(|m| m.end() <= col))
            .map(|caps| caps[0].len() - renumber(&caps).len())
            .sum();
        for line in &mut self.state.lines {
            *line = PASTE_MARKER_REGEX.replace_all(line, renumber).into_owned();
        }
        self.state.cursor_col = col - shift;
    }

    pub fn handle_forward_delete(&mut self) {
        self.exit_history_browsing();
        self.last_action = None;
        let line = self.state.cursor_line;
        let col = self.state.cursor_col;
        let line_len = self.current_line().len();

        if col < line_len {
            self.push_undo_snapshot();
            let after = &self.current_line()[col..];
            let length = self.graphemes(after).first().map_or(1, |s| s.segment.len());
            self.state.lines[line].replace_range(col..col + length, "");
        } else if line + 1 < self.state.lines.len() {
            self.push_undo_snapshot();
            let next = self.state.lines.remove(line + 1);
            self.state.lines[line].push_str(&next);
        }
        self.emit_change();

        if self.autocomplete_mode.is_some() {
            self.update_autocomplete();
        } else {
            self.retrigger_autocomplete_in_context();
        }
    }

    /// Joins the current line onto the line above, killing the newline.
    fn kill_newline_backward(&mut self) {
        self.push_undo_snapshot();
        self.kill_ring.push("\n", true, self.last_action == Some(LastAction::Kill));
        self.last_action = Some(LastAction::Kill);
        let line = self.state.cursor_line;
        let current = self.state.lines.remove(line);
        let joined_at = self.state.lines[line - 1].len();
        self.state.lines[line - 1].push_str(&current);
        self.state.cursor_line -= 1;
        self.set_cursor_col(joined_at);
    }

    /// Joins the next line onto the current one, killing the newline.
    fn kill_newline_forward(&mut self) {
        self.push_undo_snapshot();
        self.kill_ring.push("\n", false, self.last_action == Some(LastAction::Kill));
        self.last_action = Some(LastAction::Kill);
        let line = self.state.cursor_line;
        let next = self.state.lines.remove(line + 1);
        self.state.lines[line].push_str(&next);
    }

    pub fn delete_to_start_of_line(&mut self) {
        self.exit_history_browsing();
        if self.state.cursor_col > 0 {
            self.push_undo_snapshot();
            let line = self.state.cursor_line;
            let deleted: String = self.state.lines[line].drain(..self.state.cursor_col).collect();
            self.kill_ring.push(&deleted, true, self.last_action == Some(LastAction::Kill));
            self.last_action = Some(LastAction::Kill);
            self.set_cursor_col(0);
        } else if self.state.cursor_line > 0 {
            self.kill_newline_backward();
        }
        self.emit_change();
    }

    pub fn delete_to_end_of_line(&mut self) {
        self.exit_history_browsing();
        let line = self.state.cursor_line;
        if self.state.cursor_col < self.current_line().len() {
            self.push_undo_snapshot();
            let deleted = self.state.lines[line].split_off(self.state.cursor_col);
            self.kill_ring.push(&deleted, false, self.last_action == Some(LastAction::Kill));
            self.last_action = Some(LastAction::Kill);
        } else if line + 1 < self.state.lines.len() {
            self.kill_newline_forward();
        }
        self.emit_change();
    }

    pub fn delete_word_backwards(&mut self) {
        self.exit_history_browsing();
        if self.state.cursor_col == 0 {
            if self.state.cursor_line > 0 {
                self.kill_newline_backward();
            }
        } else {
            self.push_undo_snapshot();
            // Word movement clears the last action, so read it first.
            let was_kill = self.last_action == Some(LastAction::Kill);
            let old_col = self.state.cursor_col;
            self.move_word_backwards();
            let delete_from = self.state.cursor_col;
            let line = self.state.cursor_line;
            let deleted: String = self.state.lines[line].drain(delete_from..old_col).collect();
            self.kill_ring.push(&deleted, true, was_kill);
            self.last_action = Some(LastAction::Kill);
            self.set_cursor_col(delete_from);
        }
        self.emit_change();
    }

    pub fn delete_word_forward(&mut self) {
        self.exit_history_browsing();
        let line = self.state.cursor_line;
        if self.state.cursor_col >= self.current_line().len() {
            if line + 1 < self.state.lines.len() {
                self.kill_newline_forward();
            }
        } else {
            self.push_undo_snapshot();
            let was_kill = self.last_action == Some(LastAction::Kill);
            let old_col = self.state.cursor_col;
            self.move_word_forwards();
            let delete_to = self.state.cursor_col;
            self.set_cursor_col(old_col);
            let deleted: String = self.state.lines[line].drain(old_col..delete_to).collect();
            self.kill_ring.push(&deleted, false, was_kill);
            self.last_action = Some(LastAction::Kill);
        }
        self.emit_change();
    }

    pub fn yank(&mut self) {
        let Some(text) = self.kill_ring.peek().map(str::to_string) else { return };
        self.push_undo_snapshot();
        self.insert_yanked_text(&text);
        self.last_action = Some(LastAction::Yank);
    }

    /// Replaces the last yank with the next older kill ring entry (only right after a yank).
    pub fn yank_pop(&mut self) {
        if self.last_action != Some(LastAction::Yank) || self.kill_ring.len() <= 1 {
            return;
        }
        self.push_undo_snapshot();
        self.delete_yanked_text();
        self.kill_ring.rotate();
        let text = self.kill_ring.peek().unwrap_or_default().to_string();
        self.insert_yanked_text(&text);
        self.last_action = Some(LastAction::Yank);
    }

    fn insert_yanked_text(&mut self, text: &str) {
        self.exit_history_browsing();
        let pieces: Vec<&str> = text.split('\n').collect();
        let line = self.state.cursor_line;
        let col = self.state.cursor_col;
        if pieces.len() == 1 {
            self.state.lines[line].insert_str(col, text);
            self.set_cursor_col(col + text.len());
        } else {
            let after = self.state.lines[line].split_off(col);
            self.state.lines[line].push_str(pieces[0]);
            let last = pieces[pieces.len() - 1];
            let mut inserted: Vec<String> = pieces[1..pieces.len() - 1].iter().map(|s| s.to_string()).collect();
            inserted.push(format!("{last}{after}"));
            let last_index = line + pieces.len() - 1;
            self.state.lines.splice(line + 1..line + 1, inserted);
            self.state.cursor_line = last_index;
            self.set_cursor_col(last.len());
        }
        self.emit_change();
    }

    /// Removes the text inserted by the last yank, which is still the newest ring entry.
    fn delete_yanked_text(&mut self) {
        let Some(yanked) = self.kill_ring.peek().map(str::to_string) else { return };
        let pieces: Vec<&str> = yanked.split('\n').collect();
        let line = self.state.cursor_line;
        let col = self.state.cursor_col;
        if pieces.len() == 1 {
            let start = col.saturating_sub(yanked.len());
            self.state.lines[line].replace_range(start..col, "");
            self.set_cursor_col(start);
        } else {
            let Some(start_line) = (line + 1).checked_sub(pieces.len()) else { return };
            let start_col = self.state.lines[start_line].len().saturating_sub(pieces[0].len());
            let after = self.state.lines[line][col..].to_string();
            let merged = format!("{}{after}", &self.state.lines[start_line][..start_col]);
            self.state.lines.splice(start_line..start_line + pieces.len(), [merged]);
            self.state.cursor_line = start_line;
            self.set_cursor_col(start_col);
        }
        self.emit_change();
    }
}
