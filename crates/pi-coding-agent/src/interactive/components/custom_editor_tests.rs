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

use super::{CustomEditor, EditorHandle};
use crate::interactive_components::test_support::setup_with;

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
    CustomEditor::new(tui, theme, EditorOptions::default(), false).handle()
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
    let _keys = setup_with(&[("tui.select.pageDown", "ctrl+v"), ("app.clipboard.pasteImage", "ctrl+v")]);
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
    editor.handle_input("\r");

    assert_eq!(pastes.get(), 0);
    assert_eq!(*submitted.borrow(), "/command-5");

    editor.handle_input("\x16");
    assert_eq!(pastes.get(), 1);
}
