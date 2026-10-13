//! Component theme structs: re-exports of the pi-tui component types, so theme builders hand
//! their results straight to `Markdown`, `SelectList`, `SettingsList`, and `Editor`.

pub use pi_tui::components::StyleFn;
pub use pi_tui::components::editor::EditorTheme;
pub use pi_tui::components::markdown::{HighlightCodeFn, MarkdownTheme};
pub use pi_tui::components::select_list::SelectListTheme;
pub use pi_tui::components::settings_list::{SelectedStyleFn, SettingsListTheme};
