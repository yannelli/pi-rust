//! Port of upstream `custom-editor-keybindings.test.ts`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use pi_tui::autocomplete::{
    AutocompleteItem, AutocompleteSuggestions, CompletionResult, SuggestionOptions, SuggestionsFuture,
};
use pi_tui::components::editor::SelectListTheme;
use pi_tui::components::{EditorOptions, EditorTheme, StyleFn};
use pi_tui::testing::VirtualTerminal;
use pi_tui::{AutocompleteProvider, Tui, TuiOptions, ui_loop};

use super::{CustomEditor, EditorHandle, StatusIndicator};
use crate::agent_session::CompactionReason;
use crate::interactive_components::test_support::{setup, setup_with};

fn plain_theme() -> EditorTheme {
    let plain: StyleFn = Rc::new(str::to_string);
    let select_list = SelectListTheme {
        selected_prefix: plain.clone(),
        selected_text: plain.clone(),
        description: plain.clone(),
        scroll_info: plain.clone(),
        no_match: plain.clone(),
    };
    EditorTheme { border_color: plain, select_list }
}

fn editor(tui: &Tui) -> EditorHandle {
    CustomEditor::new(tui, plain_theme(), EditorOptions::default(), false).handle()
}

fn tui() -> Tui {
    Tui::new_main_screen(VirtualTerminal::shared(80, 24), TuiOptions::default())
}

#[test]
fn gives_an_explicit_history_binding_precedence_over_model_cycling() {
    let _keys = setup_with(&[("tui.editor.historyPrevious", "ctrl+p"), ("tui.editor.historyNext", "ctrl+n")]);
    let tui = tui();
    let editor = editor(&tui);
    let model_cycles = Rc::new(Cell::new(0));
    let cycles = model_cycles.clone();
    editor.on_action("app.model.cycleForward", move || cycles.set(cycles.get() + 1));
    editor.add_to_history("previous prompt");
    editor.set_text("draft");

    editor.handle_input("\x10");
    assert_eq!(editor.get_text(), "previous prompt");
    assert_eq!(model_cycles.get(), 0);

    editor.handle_input("\x0e");
    assert_eq!(editor.get_text(), "draft");
}

/// Ctrl+Enter in its Kitty and modifyOtherKeys forms.
const CTRL_ENTER: [&str; 2] = ["\x1b[13;5u", "\x1b[27;5;13~"];

/// The message actions an editor ran and the texts it submitted.
#[derive(Default)]
struct Recorded {
    actions: RefCell<Vec<&'static str>>,
    submitted: RefCell<Vec<String>>,
}

fn recording_editor(tui: &Tui) -> (EditorHandle, Rc<Recorded>) {
    let editor = editor(tui);
    let recorded = Rc::new(Recorded::default());
    for id in ["app.message.followUp", "app.message.dequeue", "app.message.sendNow", "app.message.interrupt"] {
        let recorded = recorded.clone();
        editor.on_action(id, move || recorded.actions.borrow_mut().push(id));
    }
    let sink = recorded.clone();
    editor.on_submit(move |text| sink.submitted.borrow_mut().push(text.to_string()));
    (editor, recorded)
}

#[test]
fn ctrl_enter_runs_only_the_interrupt_action() {
    let _keys = setup();
    let tui = tui();
    let (editor, recorded) = recording_editor(&tui);
    editor.set_text("draft");

    for sequence in CTRL_ENTER {
        editor.handle_input(sequence);
    }
    assert_eq!(*recorded.actions.borrow(), ["app.message.interrupt"; 2]);
    assert!(recorded.submitted.borrow().is_empty());
    assert_eq!(editor.get_text(), "draft", "no new line");

    editor.handle_input("\r");
    assert_eq!(*recorded.submitted.borrow(), ["draft"]);
    assert_eq!(recorded.actions.borrow().len(), 2);
}

#[test]
fn ctrl_enter_keeps_a_new_line_binding_from_keybindings_json() {
    let _keys = setup_with(&[("tui.input.newLine", "ctrl+enter")]);
    let tui = tui();
    let (editor, recorded) = recording_editor(&tui);
    editor.set_text("draft");

    editor.handle_input(CTRL_ENTER[0]);
    assert!(recorded.actions.borrow().is_empty());
    assert!(recorded.submitted.borrow().is_empty());
    assert_eq!(editor.get_text(), "draft\n");
}

/// Offers `/command-0` to `/command-11` and replaces the cursor line with the chosen value.
struct Commands;

impl AutocompleteProvider for Commands {
    fn get_suggestions(&self, _: &[String], _: usize, _: usize, _: SuggestionOptions) -> SuggestionsFuture {
        let items = (0..12).map(|i| AutocompleteItem::new(format!("/command-{i}"), format!("command-{i}"))).collect();
        Box::pin(std::future::ready(Some(AutocompleteSuggestions { items, prefix: "/".into() })))
    }

    fn apply_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        _cursor_col: usize,
        item: &AutocompleteItem,
        _prefix: &str,
    ) -> CompletionResult {
        let mut lines = lines.to_vec();
        lines[cursor_line] = item.value.clone();
        CompletionResult { lines, cursor_line, cursor_col: item.value.len() }
    }
}

#[test]
fn pages_an_open_autocomplete_menu_before_pasting_an_image_with_the_same_key() {
    // Bind both explicitly: the paste default is Alt+V on Windows and WSL.
    let _keys = setup_with(&[
        ("tui.select.pageDown", "ctrl+v"),
        ("tui.select.pageUp", "alt+v"),
        ("app.clipboard.pasteImage", "ctrl+v"),
    ]);
    let tui = tui();
    let editor = editor(&tui);
    let pastes = Rc::new(Cell::new(0));
    let pasted = pastes.clone();
    editor.set_on_paste_image(Some(Rc::new(move || pasted.set(pasted.get() + 1))));
    let submitted = Rc::new(RefCell::new(String::new()));
    let sink = submitted.clone();
    editor.on_submit(move |text| *sink.borrow_mut() = text.to_string());
    editor.set_autocomplete_provider(Rc::new(Commands));

    editor.handle_input("/");
    for _ in 0..4 {
        ui_loop::run_pending().expect("ui loop");
    }
    assert!(editor.editor().is_showing_autocomplete());
    editor.handle_input("\x16");
    editor.handle_input("\x16");
    editor.handle_input("\x1bv");
    editor.handle_input("\r");

    assert_eq!(pastes.get(), 0);
    assert_eq!(*submitted.borrow(), "/command-5");

    editor.handle_input("\x16");
    assert_eq!(pastes.get(), 1);
}

/// The embedded status takes the border color, like the interactive mode's spinner. A status
/// too wide for the `↑ N more` label leaves only the spinner, styled during the editor render.
#[test]
fn renders_an_embedded_status_that_reads_the_border_color_while_scrolled() {
    let tui = tui();
    let handle = CustomEditor::new(&tui, plain_theme(), EditorOptions::default(), true).handle();
    let editor = handle.editor().clone();
    let color: StyleFn = Rc::new(move |text: &str| (editor.border_color())(text));
    handle.set_working_status_indicator(Some(StatusIndicator::compaction(
        &tui,
        CompactionReason::Threshold,
        Some(color),
    )));
    handle.set_text(&["line"; 30].join("\n"));

    let top = handle.editor().render(80).remove(0);
    assert!(top.contains("↑ 23 more"), "{top}");
}
