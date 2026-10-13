//! Key handling: maps terminal input to editor actions through the `tui.editor.*`,
//! `tui.input.*`, and `tui.select.*` keybindings.

use super::state::{EditorCore, JumpDirection};
use crate::keybindings::{KeybindingsManager, get_keybindings, ids};
use crate::keys::{decode_printable_key, matches_key};

const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";

/// Editing action bound to a keybinding id.
type KeyAction = fn(&mut EditorCore);

/// Deletion, kill ring, and horizontal movement actions, in upstream order.
const EDIT_ACTIONS: &[(&str, KeyAction)] = &[
    (ids::EDITOR_DELETE_TO_LINE_END, EditorCore::delete_to_end_of_line),
    (ids::EDITOR_DELETE_TO_LINE_START, EditorCore::delete_to_start_of_line),
    (ids::EDITOR_DELETE_WORD_BACKWARD, EditorCore::delete_word_backwards),
    (ids::EDITOR_DELETE_WORD_FORWARD, EditorCore::delete_word_forward),
];

const MOVE_ACTIONS: &[(&str, KeyAction)] = &[
    (ids::EDITOR_CURSOR_LINE_START, EditorCore::move_to_line_start),
    (ids::EDITOR_CURSOR_LINE_END, EditorCore::move_to_line_end),
    (ids::EDITOR_CURSOR_WORD_LEFT, EditorCore::move_word_backwards),
    (ids::EDITOR_CURSOR_WORD_RIGHT, EditorCore::move_word_forwards),
];

fn first_code_unit_at_least_space(data: &str) -> bool {
    data.encode_utf16().next().is_some_and(|unit| unit >= 32)
}

/// Shift+Enter and the other newline encodings terminals send.
fn is_new_line(data: &str, kb: &KeybindingsManager) -> bool {
    kb.matches(data, ids::INPUT_NEW_LINE)
        || (data.starts_with('\n') && data.len() > 1)
        || data == "\x1b\r"
        || data == "\x1b[13;2~"
        || (data.len() > 1 && data.contains('\x1b') && data.contains('\r'))
        || data == "\n"
}

impl EditorCore {
    pub fn handle_input(&mut self, data: &str) {
        let kb = get_keybindings();

        // Character jump: the next printable key is the target.
        if let Some(direction) = self.jump_mode {
            if kb.matches(data, ids::EDITOR_JUMP_FORWARD) || kb.matches(data, ids::EDITOR_JUMP_BACKWARD) {
                self.jump_mode = None;
                return;
            }
            let printable =
                decode_printable_key(data).or_else(|| first_code_unit_at_least_space(data).then(|| data.to_string()));
            self.jump_mode = None;
            if let Some(target) = printable {
                self.jump_to_char(&target, direction);
                return;
            }
            // A control key cancels the jump and is handled normally.
        }

        // Bracketed paste is buffered until its end marker.
        let stripped;
        let mut data = data;
        if data.contains(PASTE_START) {
            self.is_in_paste = true;
            self.paste_buffer.clear();
            stripped = data.replacen(PASTE_START, "", 1);
            data = &stripped;
        }
        if self.is_in_paste {
            self.paste_buffer.push_str(data);
            if let Some(end) = self.paste_buffer.find(PASTE_END) {
                let content = self.paste_buffer[..end].to_string();
                let remaining = self.paste_buffer[end + PASTE_END.len()..].to_string();
                if !content.is_empty() {
                    self.handle_paste(&content);
                }
                self.is_in_paste = false;
                self.paste_buffer.clear();
                if !remaining.is_empty() {
                    self.handle_input(&remaining);
                }
            }
            return;
        }

        // Ctrl+C is left to the parent (clear or exit).
        if kb.matches(data, ids::INPUT_COPY) {
            return;
        }
        if kb.matches(data, ids::EDITOR_UNDO) {
            self.undo();
            return;
        }

        if self.autocomplete_mode.is_some()
            && self.autocomplete_list.is_some()
            && self.handle_autocomplete_key(data, &kb)
        {
            return;
        }

        if kb.matches(data, ids::INPUT_TAB) && self.autocomplete_mode.is_none() {
            self.handle_tab_completion();
            return;
        }

        if let Some((_, action)) = EDIT_ACTIONS.iter().find(|(id, _)| kb.matches(data, id)) {
            action(self);
            return;
        }
        if kb.matches(data, ids::EDITOR_DELETE_CHAR_BACKWARD) || matches_key(data, "shift+backspace") {
            self.handle_backspace();
            return;
        }
        if kb.matches(data, ids::EDITOR_DELETE_CHAR_FORWARD) || matches_key(data, "shift+delete") {
            self.handle_forward_delete();
            return;
        }
        if kb.matches(data, ids::EDITOR_YANK) {
            self.yank();
            return;
        }
        if kb.matches(data, ids::EDITOR_YANK_POP) {
            self.yank_pop();
            return;
        }

        // Dedicated history keys browse entries instead of moving the cursor.
        if kb.matches(data, ids::EDITOR_HISTORY_PREVIOUS) {
            self.cancel_autocomplete();
            self.navigate_history(-1);
            return;
        }
        if kb.matches(data, ids::EDITOR_HISTORY_NEXT) {
            self.cancel_autocomplete();
            self.navigate_history(1);
            return;
        }

        if let Some((_, action)) = MOVE_ACTIONS.iter().find(|(id, _)| kb.matches(data, id)) {
            action(self);
            return;
        }

        if is_new_line(data, &kb) {
            if self.should_submit_on_backslash_enter(data, &kb) {
                self.handle_backspace();
                self.submit_value();
                return;
            }
            self.add_new_line();
            return;
        }

        if kb.matches(data, ids::INPUT_SUBMIT) {
            if self.disable_submit {
                return;
            }
            // Terminals without Shift+Enter: a backslash before the cursor turns Enter into a newline.
            if self.char_before_cursor_is_backslash() {
                self.handle_backspace();
                self.add_new_line();
                return;
            }
            self.submit_value();
            return;
        }

        if self.handle_vertical_key(data, &kb) {
            return;
        }

        if kb.matches(data, ids::EDITOR_JUMP_FORWARD) {
            self.jump_mode = Some(JumpDirection::Forward);
            return;
        }
        if kb.matches(data, ids::EDITOR_JUMP_BACKWARD) {
            self.jump_mode = Some(JumpDirection::Backward);
            return;
        }

        if matches_key(data, "shift+space") {
            self.insert_character(" ");
            return;
        }
        if let Some(text) = decode_printable_key(data) {
            self.insert_character(&text);
            return;
        }
        if first_code_unit_at_least_space(data) {
            self.insert_character(data);
        }
    }

    /// Keys while the popup is open. Returns true when the key was consumed.
    fn handle_autocomplete_key(&mut self, data: &str, kb: &KeybindingsManager) -> bool {
        if kb.matches(data, ids::SELECT_CANCEL) {
            self.cancel_autocomplete();
            return true;
        }
        if kb.matches(data, ids::SELECT_UP) || kb.matches(data, ids::SELECT_DOWN) {
            if let Some(list) = self.autocomplete_list.as_mut() {
                list.handle_key(data);
            }
            return true;
        }
        let is_tab = kb.matches(data, ids::INPUT_TAB);
        if is_tab || kb.matches(data, ids::SELECT_CONFIRM) {
            let selected = self.autocomplete_list.as_ref().and_then(|list| list.selected_item().cloned());
            if let Some(item) = selected.filter(|_| self.provider.is_some()) {
                let is_command = self.autocomplete_prefix.starts_with('/');
                self.accept_completion(&item);
                // Enter on a slash command name also submits it.
                return is_tab || !is_command;
            }
            return is_tab;
        }
        false
    }

    /// Up, Down, Left, Right, and paging. Returns true when the key was consumed.
    fn handle_vertical_key(&mut self, data: &str, kb: &KeybindingsManager) -> bool {
        if kb.matches(data, ids::EDITOR_CURSOR_UP) {
            let on_first = self.is_on_first_visual_line();
            if on_first && (self.is_editor_empty() || self.history_index.is_some() || self.state.cursor_col == 0) {
                self.navigate_history(-1);
            } else if on_first {
                self.move_to_line_start();
            } else {
                self.move_cursor(-1, 0);
            }
        } else if kb.matches(data, ids::EDITOR_CURSOR_DOWN) {
            let on_last = self.is_on_last_visual_line();
            if self.history_index.is_some() && on_last {
                self.navigate_history(1);
            } else if on_last {
                self.move_to_line_end();
            } else {
                self.move_cursor(1, 0);
            }
        } else if kb.matches(data, ids::EDITOR_CURSOR_RIGHT) {
            self.move_cursor(0, 1);
        } else if kb.matches(data, ids::EDITOR_CURSOR_LEFT) {
            self.move_cursor(0, -1);
        } else if kb.matches(data, ids::EDITOR_PAGE_UP) {
            self.page_scroll(-1);
        } else if kb.matches(data, ids::EDITOR_PAGE_DOWN) {
            self.page_scroll(1);
        } else {
            return false;
        }
        true
    }

    /// With Shift+Enter bound to submit, a plain Enter after a backslash submits too.
    fn should_submit_on_backslash_enter(&self, data: &str, kb: &KeybindingsManager) -> bool {
        if self.disable_submit || !matches_key(data, "enter") {
            return false;
        }
        let submit_keys = kb.get_keys(ids::INPUT_SUBMIT);
        let has_shift_enter = submit_keys.iter().any(|key| key == "shift+enter" || key == "shift+return");
        has_shift_enter && self.char_before_cursor_is_backslash()
    }
}
