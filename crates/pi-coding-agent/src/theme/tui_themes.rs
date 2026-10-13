//! Component theme structs with the exact shapes the pi-tui component streams define
//! (`pi_tui::components::{markdown, select_list, settings_list, editor}`). Once those land on
//! `parity`, replace these definitions with `pub use` re-exports of the pi-tui types.

use std::rc::Rc;

pub use pi_tui::components::StyleFn;

/// Highlights a code block: `(code, language)` to rendered lines.
pub type HighlightCodeFn = Rc<dyn Fn(&str, Option<&str>) -> Vec<String>>;

/// Styles text depending on whether its row is selected.
pub type SelectedStyleFn = Rc<dyn Fn(&str, bool) -> String>;

/// Styles for the Markdown component (upstream `MarkdownTheme`).
#[derive(Clone)]
pub struct MarkdownTheme {
    pub heading: StyleFn,
    pub link: StyleFn,
    pub link_url: StyleFn,
    pub code: StyleFn,
    pub code_block: StyleFn,
    pub code_block_border: StyleFn,
    pub quote: StyleFn,
    pub quote_border: StyleFn,
    pub hr: StyleFn,
    pub list_bullet: StyleFn,
    pub bold: StyleFn,
    pub italic: StyleFn,
    pub strikethrough: StyleFn,
    pub underline: StyleFn,
    pub highlight_code: Option<HighlightCodeFn>,
    /// Prefix of each rendered code block line (default: two spaces).
    pub code_block_indent: Option<String>,
}

/// Styles for the SelectList component (upstream `SelectListTheme`).
#[derive(Clone)]
pub struct SelectListTheme {
    pub selected_prefix: StyleFn,
    pub selected_text: StyleFn,
    pub description: StyleFn,
    pub scroll_info: StyleFn,
    pub no_match: StyleFn,
}

/// Styles for the Editor component (upstream `EditorTheme`).
#[derive(Clone)]
pub struct EditorTheme {
    pub border_color: StyleFn,
    pub select_list: SelectListTheme,
}

/// Styles for the SettingsList component (upstream `SettingsListTheme`).
#[derive(Clone)]
pub struct SettingsListTheme {
    pub label: SelectedStyleFn,
    pub value: SelectedStyleFn,
    pub description: StyleFn,
    pub cursor: String,
    pub hint: StyleFn,
}
