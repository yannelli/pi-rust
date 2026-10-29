use std::cell::RefCell;
use std::rc::Rc;

use pi_tui::components::SettingsList;
use pi_tui::components::settings_list::SettingsListOptions;
use pi_tui::utils::truncate_to_width;
use pi_tui::{Component, MouseResponse, TuiMouseEvent};
use serde_json::{Value, json};

use super::editor_items::build_items;
use super::{BuiltinStatusLine, Segment, status_line_choice};
use crate::settings::schema::StatusLineSettings;
use crate::theme::{ThemeColor, get_settings_list_theme, theme};

type Changes = Rc<RefCell<Vec<(String, String)>>>;
type Save = Box<dyn FnMut(Value) -> Result<(), String>>;

pub(super) struct StatusLineEditor {
    value: Value,
    list: SettingsList,
    changes: Changes,
    save: Save,
    done: Rc<dyn Fn()>,
    error: Option<String>,
}

impl StatusLineEditor {
    pub(super) fn new(value: Option<Value>, save: Save, done: Rc<dyn Fn()>) -> Self {
        let value = value.filter(Value::is_object).unwrap_or_else(|| json!({"type": "off"}));
        let changes: Changes = Rc::default();
        let list = Self::list(&value, &changes, &done);
        Self { value, list, changes, save, done, error: None }
    }

    fn list(value: &Value, changes: &Changes, done: &Rc<dyn Fn()>) -> SettingsList {
        let changes = changes.clone();
        let done = done.clone();
        SettingsList::new(
            build_items(value),
            10,
            get_settings_list_theme(),
            move |id, value| changes.borrow_mut().push((id.into(), value.into())),
            move || done(),
            SettingsListOptions::default(),
        )
    }

    fn apply_changes(&mut self) {
        let changes = std::mem::take(&mut *self.changes.borrow_mut());
        for (id, selected) in changes {
            match edit_value(&self.value, &id, &selected).and_then(|value| {
                (self.save)(value.clone())?;
                Ok(value)
            }) {
                Ok(value) => {
                    self.value = value;
                    self.error = None;
                }
                Err(error) => self.error = Some(error),
            }
            self.list = Self::list(&self.value, &self.changes, &self.done);
            self.list.select_item(&id);
        }
    }
}

pub(super) fn edit_value(current: &Value, id: &str, selected: &str) -> Result<Value, String> {
    if id == "preset" {
        return status_line_choice(Some(current.clone()), selected);
    }
    if id == "source" && selected != "builtin" {
        super::external::validate_source(selected, &crate::config::home_dir())?;
        return status_line_choice(Some(current.clone()), selected);
    }
    let mut value = current.clone();
    value["type"] = json!("builtin");
    match id {
        "source" => {}
        "segments" => value[id] = serde_json::from_str(selected).map_err(|error| error.to_string())?,
        "padding" => value[id] = json!(selected.parse::<u8>().map_err(|error| error.to_string())?),
        "context" => {
            let settings: StatusLineSettings = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
            let builtin = BuiltinStatusLine::from_settings(&settings, 0);
            let mut lines = super::layout_editor::layout_values(&builtin);
            let mut replaced = false;
            for line in &mut lines {
                line.retain_mut(|item| {
                    let context = super::segments::Item::parse(item).is_some_and(
                        |item| matches!(item.kind, super::segments::ItemKind::Segment(segment) if is_context(segment)),
                    );
                    if !context {
                        return true;
                    }
                    if selected == "hidden" || replaced {
                        return false;
                    }
                    item["segment"] = json!(selected);
                    replaced = true;
                    true
                });
            }
            if selected != "hidden" && !replaced {
                if lines.is_empty() {
                    lines.push(Vec::new());
                }
                lines[0].push(json!(selected));
            }
            value["segments"] = json!(lines);
        }
        _ if selected == "preset" => {
            let object = value.as_object_mut().expect("settings object");
            object.remove(id);
            if id == "chip" {
                object.remove("style");
            }
        }
        _ => value[id] = json!(selected),
    }
    Ok(value)
}

pub(super) fn is_context(segment: Segment) -> bool {
    matches!(
        segment,
        Segment::Context
            | Segment::ContextUsed
            | Segment::ContextRemaining
            | Segment::ContextWindow
            | Segment::ContextBar
            | Segment::ContextTokens
            | Segment::ContextHeadroom
    )
}

impl Component for StatusLineEditor {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.apply_changes();
        let mut lines = vec![theme().fg(ThemeColor::Accent, "Status line · global settings")];
        if let Some(error) = &self.error {
            lines.push(theme().fg(ThemeColor::Error, error));
        }
        lines.extend(self.list.render(width));
        lines.into_iter().map(|line| truncate_to_width(&line, width)).collect()
    }

    fn handle_input(&mut self, data: &str) {
        self.list.handle_input(data);
        self.apply_changes();
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<MouseResponse> {
        let mut event = event.clone();
        let header = if self.error.is_some() { 2 } else { 1 };
        event.y -= header;
        event.height -= header;
        if event.y < 0 {
            return None;
        }
        let response = self.list.handle_mouse(&event);
        self.apply_changes();
        response
    }

    fn invalidate(&mut self) {
        self.list.invalidate();
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::theme::tests::global_state;

    #[test]
    fn context_choice_keeps_position_and_custom_pill_attributes() {
        let value = json!({"command": "saved.sh", "unknown": true, "segments": [
            ["dir", "fill", {"segment": "contextBar", "icon": "C", "color": "accent"}, "cost"],
            ["model", "contextUsed"]]});
        let edited = edit_value(&value, "context", "contextHeadroom").unwrap();
        assert_eq!(edited["segments"][0][2], json!({"segment": "contextHeadroom", "icon": "C", "color": "accent"}));
        assert_eq!(edited["segments"][1].as_array().unwrap().len(), 1);
        assert_eq!(edited["command"], "saved.sh");
        assert_eq!(edited["unknown"], true);
        let hidden = edit_value(&edited, "context", "hidden").unwrap();
        assert_eq!(hidden["segments"][0].as_array().unwrap().len(), 3);
        let reset = edit_value(&json!({"chip": "block", "style": "rounded"}), "chip", "preset").unwrap();
        assert!(reset.get("chip").is_none() && reset.get("style").is_none());
    }

    #[test]
    fn keyboard_selection_saves_and_escape_closes() {
        let _theme = global_state();
        let saved = Rc::new(RefCell::new(Vec::new()));
        let writes = saved.clone();
        let closed = Rc::new(Cell::new(false));
        let flag = closed.clone();
        let mut editor = StatusLineEditor::new(
            None,
            Box::new(move |value| {
                writes.borrow_mut().push(value);
                Ok(())
            }),
            Rc::new(move || flag.set(true)),
        );
        editor.list.select_item("preset");
        editor.handle_input("\r");
        editor.handle_input("compact");
        editor.handle_input("\r");
        assert_eq!(saved.borrow().len(), 1);
        assert_eq!(saved.borrow()[0]["preset"], "compact");
        assert!(!closed.get());
        editor.handle_input("\x1b");
        assert!(closed.get());
    }

    #[test]
    fn module_editor_applies_changes_only_when_requested() {
        let _theme = global_state();
        let saved = Rc::new(RefCell::new(Vec::new()));
        let writes = saved.clone();
        let mut editor = StatusLineEditor::new(
            Some(json!({"segments": ["model"]})),
            Box::new(move |value| {
                writes.borrow_mut().push(value);
                Ok(())
            }),
            Rc::new(|| {}),
        );
        editor.list.select_item("segments");
        for key in ["\r", "\r", "remove", "\r", "\x1b"] {
            editor.handle_input(key);
        }
        assert!(saved.borrow().is_empty());
        for key in ["\r", "\r", "remove", "\r", "\r", "contextRemaining", "\r", "\x1b[B", "\x1b[B", "\r"] {
            editor.handle_input(key);
        }
        assert_eq!(saved.borrow().len(), 1);
        assert_eq!(saved.borrow()[0]["segments"], json!([["contextRemaining"]]));
    }

    #[test]
    fn rejected_save_keeps_original_settings_and_reports_error() {
        let _theme = global_state();
        let original = json!({"preset": "minimal"});
        let mut editor =
            StatusLineEditor::new(Some(original.clone()), Box::new(|_| Err("read only".into())), Rc::new(|| {}));
        editor.changes.borrow_mut().push(("preset".into(), "compact".into()));
        editor.apply_changes();
        assert_eq!(editor.value, original);
        assert_eq!(editor.error.as_deref(), Some("read only"));
    }

    #[test]
    fn layout_actions_preserve_objects_and_can_remove_all_modules() {
        use super::super::layout_editor::change_layout;
        let pill = json!({"segment": "model", "color": "#ffffff", "icon": "M"});
        let mut lines = vec![vec![json!("dir"), pill.clone()]];
        change_layout(&mut lines, "0:1", "earlier");
        assert_eq!(lines[0][0], pill);
        change_layout(&mut lines, "0:0", "next row");
        assert_eq!(lines[1], vec![pill.clone()]);
        change_layout(&mut lines, "add:1", "contextHeadroom");
        change_layout(&mut lines, "1:0", "later");
        assert_eq!(lines[1], vec![json!("contextHeadroom"), pill]);
        change_layout(&mut lines, "1:0", "previous row");
        assert_eq!(lines[0], vec![json!("dir"), json!("contextHeadroom")]);
        change_layout(&mut lines, "1:0", "remove");
        change_layout(&mut lines, "0:1", "remove");
        change_layout(&mut lines, "0:0", "remove");
        assert!(lines.iter().all(Vec::is_empty));
    }
}
