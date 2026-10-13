//! Port of upstream `editor-history-keybindings.test.ts`. It replaces the process-wide
//! keybindings, so it lives in its own test binary.

mod editor_common;

use editor_common::{cursor, editor};
use pi_tui::keybindings::{KeybindingsConfig, KeybindingsManager, ids, set_keybindings, tui_keybindings};

#[test]
fn browses_history_directly_without_moving_the_cursor_first() {
    let mut user = KeybindingsConfig::new();
    user.insert(ids::EDITOR_HISTORY_PREVIOUS, "ctrl+p");
    user.insert(ids::EDITOR_HISTORY_NEXT, "ctrl+n");
    set_keybindings(KeybindingsManager::new(tui_keybindings(), user));

    let editor = editor();
    editor.add_to_history("older prompt");
    editor.add_to_history("newer\nmultiline prompt");
    editor.set_text("draft");
    editor.handle_input("\x1b[D");
    editor.handle_input("\x1b[D");

    editor.handle_input("\x10");
    assert_eq!(editor.get_text(), "newer\nmultiline prompt");
    assert_eq!(cursor(&editor), (0, 0));

    editor.handle_input("\x10");
    assert_eq!(editor.get_text(), "older prompt");

    editor.handle_input("\x0e");
    assert_eq!(editor.get_text(), "newer\nmultiline prompt");
    assert_eq!(cursor(&editor), (1, 16));

    editor.handle_input("\x0e");
    assert_eq!(editor.get_text(), "draft");
    assert_eq!(cursor(&editor), (0, 3));

    set_keybindings(KeybindingsManager::default());
}
