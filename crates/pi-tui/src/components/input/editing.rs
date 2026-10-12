//! Editing operations: insertion, deletion, kill and yank, undo, and cursor movement.

use super::{Input, InputState, LastAction, floor_char_boundary};
use crate::utils::{graphemes, is_whitespace_char};
use crate::word_navigation::{WordNavigationOptions, find_word_backward, find_word_forward};

impl Input {
    /// Byte length of the grapheme before the cursor (0 at the start).
    fn grapheme_len_before_cursor(&self) -> usize {
        graphemes(&self.value[..self.cursor]).next_back().map_or(0, str::len)
    }

    /// Byte length of the grapheme after the cursor (0 at the end).
    fn grapheme_len_after_cursor(&self) -> usize {
        graphemes(&self.value[self.cursor..]).next().map_or(0, str::len)
    }

    fn insert_at_cursor(&mut self, text: &str) {
        self.value.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    /// Inserts typed text. Consecutive word characters coalesce into one undo unit.
    pub(super) fn insert_character(&mut self, text: &str) {
        if is_whitespace_char(text) || self.last_action != Some(LastAction::TypeWord) {
            self.push_undo();
        }
        self.last_action = Some(LastAction::TypeWord);
        self.insert_at_cursor(text);
    }

    pub(super) fn handle_backspace(&mut self) {
        self.last_action = None;
        if self.cursor > 0 {
            self.push_undo();
            let length = self.grapheme_len_before_cursor();
            self.value.replace_range(self.cursor - length..self.cursor, "");
            self.cursor -= length;
        }
    }

    pub(super) fn handle_forward_delete(&mut self) {
        self.last_action = None;
        if self.cursor < self.value.len() {
            self.push_undo();
            let length = self.grapheme_len_after_cursor();
            self.value.replace_range(self.cursor..self.cursor + length, "");
        }
    }

    pub(super) fn delete_to_line_start(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.push_undo();
        let deleted: String = self.value.drain(..self.cursor).collect();
        self.kill_ring.push(&deleted, true, self.last_action == Some(LastAction::Kill));
        self.last_action = Some(LastAction::Kill);
        self.cursor = 0;
    }

    pub(super) fn delete_to_line_end(&mut self) {
        if self.cursor >= self.value.len() {
            return;
        }
        self.push_undo();
        let deleted = self.value.split_off(self.cursor);
        self.kill_ring.push(&deleted, false, self.last_action == Some(LastAction::Kill));
        self.last_action = Some(LastAction::Kill);
    }

    pub(super) fn delete_word_backward(&mut self) {
        if self.cursor == 0 {
            return;
        }
        // Word movement clears the last action, so read it first.
        let was_kill = self.last_action == Some(LastAction::Kill);
        self.push_undo();

        let old_cursor = self.cursor;
        self.move_word_backward();
        let delete_from = self.cursor;

        let deleted: String = self.value.drain(delete_from..old_cursor).collect();
        self.kill_ring.push(&deleted, true, was_kill);
        self.last_action = Some(LastAction::Kill);
        self.cursor = delete_from;
    }

    pub(super) fn delete_word_forward(&mut self) {
        if self.cursor >= self.value.len() {
            return;
        }
        // Word movement clears the last action, so read it first.
        let was_kill = self.last_action == Some(LastAction::Kill);
        self.push_undo();

        let old_cursor = self.cursor;
        self.move_word_forward();
        let delete_to = self.cursor;
        self.cursor = old_cursor;

        let deleted: String = self.value.drain(old_cursor..delete_to).collect();
        self.kill_ring.push(&deleted, false, was_kill);
        self.last_action = Some(LastAction::Kill);
    }

    pub(super) fn yank(&mut self) {
        let Some(text) = self.kill_ring.peek().filter(|text| !text.is_empty()).map(str::to_string) else {
            return;
        };
        self.push_undo();
        self.insert_at_cursor(&text);
        self.last_action = Some(LastAction::Yank);
    }

    pub(super) fn yank_pop(&mut self) {
        if self.last_action != Some(LastAction::Yank) || self.kill_ring.len() <= 1 {
            return;
        }
        self.push_undo();

        // The last yanked text is still the newest ring entry and sits right before the cursor.
        let yanked_len = self.kill_ring.peek().map_or(0, str::len);
        let start = floor_char_boundary(&self.value, self.cursor.saturating_sub(yanked_len));
        self.value.replace_range(start..self.cursor, "");
        self.cursor = start;

        self.kill_ring.rotate();
        let text = self.kill_ring.peek().unwrap_or_default().to_string();
        self.insert_at_cursor(&text);
        self.last_action = Some(LastAction::Yank);
    }

    fn push_undo(&mut self) {
        self.undo_stack.push(&InputState { value: self.value.clone(), cursor: self.cursor });
    }

    pub(super) fn undo(&mut self) {
        let Some(snapshot) = self.undo_stack.pop() else {
            return;
        };
        self.value = snapshot.value;
        self.cursor = snapshot.cursor;
        self.last_action = None;
    }

    pub(super) fn move_left(&mut self) {
        self.last_action = None;
        self.cursor -= self.grapheme_len_before_cursor();
    }

    pub(super) fn move_right(&mut self) {
        self.last_action = None;
        self.cursor += self.grapheme_len_after_cursor();
    }

    pub(super) fn move_line_start(&mut self) {
        self.last_action = None;
        self.cursor = 0;
    }

    pub(super) fn move_line_end(&mut self) {
        self.last_action = None;
        self.cursor = self.value.len();
    }

    pub(super) fn move_word_backward(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.last_action = None;
        self.cursor = find_word_backward(&self.value, self.cursor, &WordNavigationOptions::default());
    }

    pub(super) fn move_word_forward(&mut self) {
        if self.cursor >= self.value.len() {
            return;
        }
        self.last_action = None;
        self.cursor = find_word_forward(&self.value, self.cursor, &WordNavigationOptions::default());
    }

    /// Inserts pasted text as one undo unit, dropping line breaks and expanding tabs to 4 spaces.
    pub(super) fn handle_paste(&mut self, pasted: &str) {
        self.last_action = None;
        self.push_undo();
        let clean = pasted.replace(['\r', '\n'], "").replace('\t', "    ");
        self.insert_at_cursor(&clean);
    }
}
