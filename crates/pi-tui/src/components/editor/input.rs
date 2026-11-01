//! Key handling: maps terminal input to editor actions through the `tui.editor.*`,
//! `tui.input.*`, and `tui.select.*` keybindings.

use std::time::Instant;

use super::state::{EditorCore, JumpDirection};
use crate::keybindings::{KeybindingsManager, get_keybindings, ids};
use crate::keys::{decode_printable_key, matches_key};
use crate::stdin_buffer::{PASTE_IDLE_TIMEOUT, PASTE_MAX_BYTES};

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

/// Text that a key or a chunk of typed input inserts, if any.
pub(super) fn printable_input(data: &str) -> Option<String> {
    if matches_key(data, "shift+space") {
        return Some(" ".to_string());
    }
    decode_printable_key(data).or_else(|| first_code_unit_at_least_space(data).then(|| data.to_string()))
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
    /// Inserts the first `len` bytes of the paste buffer and leaves paste mode.
    fn end_paste(&mut self, len: usize) {
        let mut content = std::mem::take(&mut self.paste_buffer);
        content.truncate(len);
        self.is_in_paste = false;
        self.paste_input_at = None;
        if self.history_search.is_some() {
            self.append_to_search(&content);
        } else if !content.is_empty() {
            self.handle_paste(&content);
        }
    }

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

        // Bracketed paste is buffered until its end marker. Without one, it ends after an idle
        // timeout or at a size cap, so that later keys such as Ctrl+C work again.
        let now = Instant::now();
        if self.paste_input_at.is_some_and(|at| now.saturating_duration_since(at) >= PASTE_IDLE_TIMEOUT) {
            self.end_paste(self.paste_buffer.len());
        }
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
            self.paste_input_at = Some(now);
            if let Some(end) = self.paste_buffer.find(PASTE_END) {
                let remaining = self.paste_buffer[end + PASTE_END.len()..].to_string();
                self.end_paste(end);
                if !remaining.is_empty() {
                    self.handle_input(&remaining);
                }
            } else if self.paste_buffer.len() > PASTE_MAX_BYTES {
                self.end_paste(self.paste_buffer.len());
            }
            return;
        }

        if self.history_search.is_some() && self.handle_history_search_key(data, &kb) {
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
        if kb.matches(data, ids::EDITOR_HISTORY_SEARCH) {
            self.start_history_search();
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

        if let Some(text) = printable_input(data) {
            self.insert_character(&text);
        }
    }

    /// Keys while the popup is open. Returns true when the key was consumed.
    fn handle_autocomplete_key(&mut self, data: &str, kb: &KeybindingsManager) -> bool {
        if kb.matches(data, ids::SELECT_CANCEL) {
            self.cancel_autocomplete();
            return true;
        }
        let moves = [ids::SELECT_UP, ids::SELECT_DOWN, ids::SELECT_PAGE_UP, ids::SELECT_PAGE_DOWN];
        if moves.iter().any(|id| kb.matches(data, id)) {
            if let Some(list) = self.autocomplete_list.as_mut() {
                list.handle_key(data);
            }
            return true;
        }
        let is_tab = kb.matches(data, ids::INPUT_TAB);
        if is_tab || kb.matches(data, ids::SELECT_CONFIRM) {
            // Text typed since the list was built makes it stale; the key then acts as without it.
            if !self.text_before_cursor().ends_with(self.autocomplete_prefix.as_str()) {
                self.cancel_autocomplete();
                return false;
            }
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

#[cfg(test)]
mod tests {
    use std::rc::Rc;
    use std::time::Instant;

    use crate::components::editor::{Editor, EditorOptions, EditorTheme, SelectListTheme};
    use crate::components::scroll_view::StyleFn;
    use crate::stdin_buffer::PASTE_IDLE_TIMEOUT;
    use crate::testing::VirtualTerminal;
    use crate::{Tui, TuiOptions};

    fn editor() -> (Editor, Tui) {
        let tui = Tui::new_main_screen(VirtualTerminal::shared(80, 24), TuiOptions::default());
        let plain: StyleFn = Rc::new(str::to_string);
        let select_list = SelectListTheme {
            selected_prefix: plain.clone(),
            selected_text: plain.clone(),
            description: plain.clone(),
            scroll_info: plain.clone(),
            no_match: plain.clone(),
        };
        let theme = EditorTheme { border_color: plain, select_list };
        (Editor::new(&tui, theme, EditorOptions::default()), tui)
    }

    /// Upstream issue 11383: a lost end marker must not swallow later keys.
    #[test]
    fn ends_a_paste_without_end_marker_after_the_idle_timeout() {
        let (editor, _tui) = editor();
        editor.handle_input("\x1b[200~partial");
        editor.handle_input(" more");
        assert_eq!(editor.get_text(), "");
        let idle_since = Instant::now().checked_sub(PASTE_IDLE_TIMEOUT).expect("clock after boot");
        editor.shared.core.borrow_mut().paste_input_at = Some(idle_since);
        editor.handle_input("x");
        assert_eq!(editor.get_text(), "partial morex");
        assert!(!editor.shared.core.borrow().is_in_paste);
    }
}
