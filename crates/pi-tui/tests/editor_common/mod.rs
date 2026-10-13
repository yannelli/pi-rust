//! Helpers shared by the editor and autocomplete test ports.
#![allow(dead_code)]

use std::cell::RefCell;
use std::ops::Deref;
use std::rc::Rc;
use std::time::Duration;

use pi_tui::autocomplete::{
    AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions, CompletionResult, SuggestionOptions,
    SuggestionsFuture,
};
use pi_tui::components::editor::{Editor, EditorOptions, EditorTheme, SelectListTheme};
use pi_tui::testing::VirtualTerminal;
use pi_tui::{Tui, TuiOptions, ui_loop};

fn style(open: &'static str, close: &'static str) -> pi_tui::components::scroll_view::StyleFn {
    Rc::new(move |text: &str| format!("{open}{text}{close}"))
}

/// Upstream `defaultEditorTheme` (chalk at level 3).
pub fn default_theme() -> EditorTheme {
    EditorTheme {
        border_color: style("\x1b[2m", "\x1b[22m"),
        select_list: SelectListTheme {
            selected_prefix: style("\x1b[34m", "\x1b[39m"),
            selected_text: style("\x1b[1m", "\x1b[22m"),
            description: style("\x1b[2m", "\x1b[22m"),
            scroll_info: style("\x1b[2m", "\x1b[22m"),
            no_match: style("\x1b[2m", "\x1b[22m"),
        },
    }
}

/// A theme that adds no styling.
pub fn plain_theme() -> EditorTheme {
    let plain: pi_tui::components::scroll_view::StyleFn = Rc::new(str::to_string);
    EditorTheme {
        border_color: plain.clone(),
        select_list: SelectListTheme {
            selected_prefix: plain.clone(),
            selected_text: plain.clone(),
            description: plain.clone(),
            scroll_info: plain.clone(),
            no_match: plain,
        },
    }
}

/// An editor on a main-screen TUI over a virtual terminal (upstream `createTestTUI`).
pub struct TestEditor {
    pub editor: Editor,
    pub tui: Tui,
}

impl Deref for TestEditor {
    type Target = Editor;

    fn deref(&self) -> &Editor {
        &self.editor
    }
}

pub fn create_tui(columns: usize, rows: usize) -> Tui {
    Tui::new_main_screen(VirtualTerminal::shared(columns, rows), TuiOptions::default())
}

pub fn editor_with(columns: usize, theme: EditorTheme, options: EditorOptions) -> TestEditor {
    let tui = create_tui(columns, 24);
    TestEditor { editor: Editor::new(&tui, theme, options), tui }
}

pub fn editor() -> TestEditor {
    editor_with(80, default_theme(), EditorOptions::default())
}

pub fn editor_cols(columns: usize) -> TestEditor {
    editor_with(columns, default_theme(), EditorOptions::default())
}

pub fn cursor(editor: &Editor) -> (usize, usize) {
    let cursor = editor.get_cursor();
    (cursor.line, cursor.col)
}

pub fn type_text(editor: &Editor, text: &str) {
    for c in text.chars() {
        editor.handle_input(c.encode_utf8(&mut [0; 4]));
    }
}

/// Upstream `flushAutocomplete`: lets queued autocomplete work run.
pub fn flush() {
    for _ in 0..4 {
        ui_loop::run_pending().expect("ui loop");
    }
}

/// Lets the 20 ms symbol debounce elapse, then flushes.
pub fn flush_debounce() {
    ui_loop::run_for(Duration::from_millis(30)).expect("ui loop");
    flush();
}

pub const UNDO: &str = "\x1b[45;5u";

/// Upstream test `applyCompletion`: replaces the prefix with the item value.
pub fn apply_completion(
    lines: &[String],
    cursor_line: usize,
    cursor_col: usize,
    item: &AutocompleteItem,
    prefix: &str,
) -> CompletionResult {
    let line = &lines[cursor_line];
    let start = cursor_col - prefix.len();
    let mut new_lines = lines.to_vec();
    new_lines[cursor_line] = format!("{}{}{}", &line[..start], item.value, &line[cursor_col..]);
    CompletionResult { lines: new_lines, cursor_line, cursor_col: start + item.value.len() }
}

pub type SuggestFn = Box<dyn Fn(&[String], usize, usize, &SuggestionOptions) -> Option<AutocompleteSuggestions>>;
pub type ApplyFn = Box<dyn Fn(&[String], usize, usize, &AutocompleteItem, &str) -> CompletionResult>;

/// A provider built from closures that answers right away (upstream mock providers).
pub struct MockProvider {
    pub triggers: Vec<String>,
    pub suggest: SuggestFn,
    pub apply: ApplyFn,
}

impl MockProvider {
    pub fn new(
        suggest: impl Fn(&[String], usize, usize, &SuggestionOptions) -> Option<AutocompleteSuggestions> + 'static,
    ) -> Self {
        Self { triggers: Vec::new(), suggest: Box::new(suggest), apply: Box::new(apply_completion) }
    }

    pub fn with_triggers(mut self, triggers: &[&str]) -> Self {
        self.triggers = triggers.iter().map(|t| t.to_string()).collect();
        self
    }

    pub fn with_apply(
        mut self,
        apply: impl Fn(&[String], usize, usize, &AutocompleteItem, &str) -> CompletionResult + 'static,
    ) -> Self {
        self.apply = Box::new(apply);
        self
    }

    pub fn shared(self) -> Rc<dyn AutocompleteProvider> {
        Rc::new(self)
    }
}

impl AutocompleteProvider for MockProvider {
    fn trigger_characters(&self) -> Vec<String> {
        self.triggers.clone()
    }

    fn get_suggestions(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        options: SuggestionOptions,
    ) -> SuggestionsFuture {
        Box::pin(std::future::ready((self.suggest)(lines, cursor_line, cursor_col, &options)))
    }

    fn apply_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> CompletionResult {
        (self.apply)(lines, cursor_line, cursor_col, item, prefix)
    }
}

/// Text before the cursor on the cursor line.
pub fn before_cursor(lines: &[String], cursor_line: usize, cursor_col: usize) -> String {
    lines[cursor_line][..cursor_col].to_string()
}

pub fn items(values: &[&str]) -> Vec<AutocompleteItem> {
    values.iter().map(|value| AutocompleteItem::new(*value, *value)).collect()
}

pub fn suggestions(values: &[&str], prefix: &str) -> Option<AutocompleteSuggestions> {
    Some(AutocompleteSuggestions { items: items(values), prefix: prefix.to_string() })
}

/// Records the text before the cursor for each request.
pub fn recorder() -> Rc<RefCell<Vec<String>>> {
    Rc::new(RefCell::new(Vec::new()))
}

/// Rendered lines with escape sequences removed.
pub fn plain_lines(lines: &[String]) -> Vec<String> {
    lines.iter().map(|line| pi_tui::strip_terminal_sequences(line)).collect()
}
