//! `/statusline [choice]`: shows or switches the status line in global settings.

use super::external::{claude_settings_path, codex_config_path, cursor_config_path, read_codex_items, read_command};
use super::{status_line_choice, status_line_choices, status_line_name};
use crate::interactive::mode::InteractiveMode;

impl InteractiveMode {
    pub(in crate::interactive) fn handle_statusline_command(&self, choice: Option<&str>) {
        let settings = self.settings();
        let Some(choice) = choice else {
            let name = status_line_name(settings.status_line().as_ref());
            self.show_status(&format!("Status line: {name}. Use /statusline {}.", status_line_choices()));
            return;
        };
        let home = crate::config::home_dir();
        let missing = match choice {
            "from-claude" => read_command(&claude_settings_path(&home)).is_none().then(|| claude_settings_path(&home)),
            "from-cursor" => read_command(&cursor_config_path(&home)).is_none().then(|| cursor_config_path(&home)),
            "from-codex" => read_codex_items(&codex_config_path(&home)).is_none().then(|| codex_config_path(&home)),
            _ => None,
        };
        if let Some(path) = missing {
            self.show_error(&format!("No status line found in {}.", path.display()));
            return;
        }
        let current = settings.global_settings().status_line.and_then(|current| serde_json::to_value(current).ok());
        match status_line_choice(current, choice) {
            Ok(value) => {
                settings.set_global_value("statusLine", Some(value));
                self.apply_runtime_settings();
                self.request_render();
                self.show_status(&format!("Status line: {choice}"));
            }
            Err(message) => self.show_error(&message),
        }
    }
}
