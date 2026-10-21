use std::rc::Rc;

use pi_tui::components::editor::SelectListTheme;
use pi_tui::components::{EditorOptions, EditorTheme, StyleFn};
use pi_tui::testing::VirtualTerminal;
use pi_tui::{Tui, TuiOptions};

use super::{PROMPT_HISTORY_FILE, PromptHistoryFile};
use crate::interactive::components::{CustomEditor, EditorHandle};
use crate::interactive_components::test_support::setup;

fn file_lines(file: &PromptHistoryFile) -> Vec<String> {
    std::fs::read_to_string(file.path()).unwrap().lines().map(str::to_string).collect()
}

#[test]
fn appends_json_lines_and_loads_them_oldest_first() {
    let dir = tempfile::tempdir().unwrap();
    let file = PromptHistoryFile::new(dir.path(), 10);
    for prompt in ["first", "  second\nwith \"quotes\"  ", "", "   ", "dritte ü"] {
        file.append(prompt).unwrap();
    }
    assert_eq!(file.path(), dir.path().join(PROMPT_HISTORY_FILE));
    assert_eq!(file_lines(&file), [r#""first""#, r#""second\nwith \"quotes\"""#, r#""dritte ü""#]);
    assert_eq!(file.load(), ["first", "second\nwith \"quotes\"", "dritte ü"]);
}

#[test]
fn a_missing_file_loads_as_empty() {
    let dir = tempfile::tempdir().unwrap();
    assert!(PromptHistoryFile::new(dir.path(), 10).load().is_empty());
}

#[test]
fn load_keeps_the_newest_entries_and_trims_a_long_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = PromptHistoryFile::new(dir.path(), 3);
    for index in 0..6 {
        file.append(&format!("prompt {index}")).unwrap();
    }
    assert_eq!(file.load(), ["prompt 3", "prompt 4", "prompt 5"]);
    assert_eq!(file_lines(&file).len(), 6, "up to twice the limit stays on disk");

    file.append("prompt 6").unwrap();
    assert_eq!(file.load(), ["prompt 4", "prompt 5", "prompt 6"]);
    assert_eq!(file_lines(&file), [r#""prompt 4""#, r#""prompt 5""#, r#""prompt 6""#]);
    assert!(!dir.path().join(format!("{PROMPT_HISTORY_FILE}.lock")).exists());
}

#[test]
fn skips_malformed_lines_and_repairs_a_torn_last_line() {
    let dir = tempfile::tempdir().unwrap();
    let file = PromptHistoryFile::new(dir.path(), 10);
    let text = "\"good one\"\nnot json\n{\"text\": \"object\"}\n42\n\n\"good two\"\n\"torn";
    std::fs::write(file.path(), text).unwrap();
    assert_eq!(file.load(), ["good one", "good two"]);
    file.append("after the tear").unwrap();
    assert_eq!(file.load(), ["good one", "good two", "after the tear"]);
}

#[test]
fn collapses_consecutive_duplicates_only() {
    let dir = tempfile::tempdir().unwrap();
    let file = PromptHistoryFile::new(dir.path(), 10);
    for prompt in ["run tests", "run tests", " run tests ", "deploy", "run tests"] {
        file.append(prompt).unwrap();
    }
    assert_eq!(file.load(), ["run tests", "deploy", "run tests"]);
}

#[test]
fn zero_entries_records_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let file = PromptHistoryFile::new(dir.path(), 0);
    file.append("anything").unwrap();
    assert!(!file.path().exists());
}

#[cfg(unix)]
#[test]
fn creates_the_file_readable_only_by_the_owner() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let file = PromptHistoryFile::new(dir.path(), 10);
    file.append("private").unwrap();
    let mode = std::fs::metadata(file.path()).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

fn editor(tui: &Tui) -> EditorHandle {
    let plain: StyleFn = Rc::new(str::to_string);
    let select_list = SelectListTheme {
        selected_prefix: plain.clone(),
        selected_text: plain.clone(),
        description: plain.clone(),
        scroll_info: plain.clone(),
        no_match: plain.clone(),
    };
    let theme = EditorTheme { border_color: plain, select_list };
    let handle = CustomEditor::new(tui, theme, EditorOptions::default(), false).handle();
    let recorder = handle.clone();
    handle.on_submit(move |text| recorder.record_prompt(text));
    handle
}

fn type_and_submit(editor: &EditorHandle, text: &str) {
    for c in text.chars() {
        editor.handle_input(c.encode_utf8(&mut [0; 4]));
    }
    editor.handle_input("\r");
}

#[test]
fn input_with_a_leading_space_stays_out_of_the_file() {
    let _keys = setup();
    let dir = tempfile::tempdir().unwrap();
    let tui = Tui::new_main_screen(VirtualTerminal::shared(80, 24), TuiOptions::default());
    let first = editor(&tui);
    first.set_prompt_history(PromptHistoryFile::new(dir.path(), 10));
    type_and_submit(&first, "visible prompt");
    type_and_submit(&first, " secret prompt");
    type_and_submit(&first, "another visible");
    first.editor().handle_input("\x1b[A");
    first.editor().handle_input("\x1b[A");
    assert_eq!(first.get_text(), "secret prompt", "the session still remembers it");

    let file = PromptHistoryFile::new(dir.path(), 10);
    assert_eq!(file.load(), ["visible prompt", "another visible"]);
    let second = editor(&tui);
    second.set_prompt_history(file);
    second.editor().handle_input("\x1b[A");
    assert_eq!(second.get_text(), "another visible");
    second.editor().handle_input("\x1b[A");
    assert_eq!(second.get_text(), "visible prompt");
}
