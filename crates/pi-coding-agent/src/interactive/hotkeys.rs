//! The `/hotkeys` table (upstream `handleHotkeysCommand`), built from the active keybindings.

use crate::interactive_components::key_display_text as key;

fn table(title: &str, rows: &[(String, &str)]) -> String {
    let mut text = format!("**{title}**\n| Key | Action |\n|-----|--------|\n");
    for (keys, action) in rows {
        text.push_str(&format!("| {keys} | {action} |\n"));
    }
    text
}

fn code(id: &str) -> String {
    format!("`{}`", key(id))
}

fn codes(ids: &[&str]) -> String {
    ids.iter().map(|id| code(id)).collect::<Vec<_>>().join(" / ")
}

/// Navigation, editing, and app shortcuts as Markdown tables.
pub fn hotkeys_markdown() -> String {
    let navigation = [
        (
            codes(&["tui.editor.cursorUp", "tui.editor.cursorDown", "tui.editor.cursorLeft", "tui.editor.cursorRight"]),
            "Move cursor / browse history",
        ),
        (code("tui.editor.historySearch"), "Search prompt history"),
        (codes(&["tui.editor.cursorWordLeft", "tui.editor.cursorWordRight"]), "Move by word"),
        (code("tui.editor.cursorLineStart"), "Start of line"),
        (code("tui.editor.cursorLineEnd"), "End of line"),
        (code("tui.editor.jumpForward"), "Jump forward to character"),
        (code("tui.editor.jumpBackward"), "Jump backward to character"),
        (codes(&["tui.editor.pageUp", "tui.editor.pageDown"]), "Scroll by page"),
    ];
    let new_line = if cfg!(windows) { "New line (Ctrl+Enter on Windows Terminal)" } else { "New line" };
    let editing = [
        (code("tui.input.submit"), "Send message"),
        (code("tui.input.newLine"), new_line),
        (code("tui.editor.deleteWordBackward"), "Delete word backwards"),
        (code("tui.editor.deleteWordForward"), "Delete word forwards"),
        (code("tui.editor.deleteToLineStart"), "Delete to start of line"),
        (code("tui.editor.deleteToLineEnd"), "Delete to end of line"),
        (code("tui.editor.yank"), "Paste the most-recently-deleted text"),
        (code("tui.editor.yankPop"), "Cycle through the deleted text after pasting"),
        (code("tui.editor.undo"), "Undo"),
    ];
    let other = [
        (code("tui.input.tab"), "Path completion / accept autocomplete"),
        (code("app.interrupt"), "Cancel autocomplete / abort streaming / clear draft"),
        (code("app.clear"), "Clear editor (first) / exit (second)"),
        (code("app.exit"), "Exit (when editor is empty)"),
        (code("app.suspend"), "Suspend to background"),
        (code("app.thinking.cycle"), "Cycle thinking level"),
        (codes(&["app.model.cycleForward", "app.model.cycleBackward"]), "Cycle models"),
        (code("app.model.select"), "Open model selector"),
        (code("app.tools.expand"), "Toggle tool output expansion"),
        (code("app.thinking.toggle"), "Toggle thinking block visibility"),
        (code("app.editor.external"), "Edit message in external editor"),
        (code("app.message.copy"), "Copy selection or last assistant message"),
        (code("app.message.followUp"), "Queue follow-up message"),
        (code("app.message.dequeue"), "Restore queued messages"),
        (code("app.clipboard.pasteImage"), "Paste files on macOS, images, or text from clipboard"),
        ("`/`".to_string(), "Slash commands"),
        ("`!`".to_string(), "Run bash command"),
        ("`!!`".to_string(), "Run bash command (excluded from context)"),
    ];
    [table("Navigation", &navigation), table("Editing", &editing), table("Other", &other)].join("\n").trim().to_string()
}
