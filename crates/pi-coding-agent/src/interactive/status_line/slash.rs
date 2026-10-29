//! `/statusline [choice]`: edit or switch the status line in global settings.

use std::cell::RefCell;
use std::rc::Rc;

use super::editor::StatusLineEditor;
use super::external::validate_source;
use super::status_line_choice;
use crate::interactive::mode::InteractiveMode;
use crate::interactive::selector::SelectorSpec;

impl InteractiveMode {
    fn show_statusline_editor(&self) {
        let weak = self.weak.clone();
        let current = self.settings().global_status_line_value();
        self.selector.show(Box::new(move |done| {
            let editor = StatusLineEditor::new(
                current,
                Box::new(move |value| {
                    let mode = weak.upgrade().ok_or("Session closed")?;
                    mode.settings().set_status_line(value)?;
                    mode.apply_runtime_settings();
                    mode.request_render();
                    Ok(())
                }),
                done,
            );
            SelectorSpec::new(Rc::new(RefCell::new(editor)))
        }));
    }

    pub(in crate::interactive) fn handle_statusline_command(&self, choice: Option<&str>) {
        let settings = self.settings();
        let choice = match choice {
            None | Some("edit") => return self.show_statusline_editor(),
            Some(choice) => choice,
        };
        if let Err(error) = validate_source(choice, &crate::config::home_dir()) {
            self.show_error(&error);
            return;
        }
        let current = settings.global_status_line_value();
        match status_line_choice(current, choice) {
            Ok(value) => {
                if let Err(error) = settings.set_status_line(value) {
                    self.show_error(&error);
                    return;
                }
                self.apply_runtime_settings();
                self.request_render();
                self.show_status(&format!("Status line: {choice}"));
            }
            Err(message) => self.show_error(&message),
        }
    }
}
