use std::cell::RefCell;
use std::rc::Rc;

use pi_tui::components::SettingsList;
use pi_tui::components::settings_list::{SettingItem, SettingsListOptions, SubmenuDone};
use pi_tui::{Component, MouseResponse, TuiMouseEvent};
use serde_json::{Value, json};

use super::editor_items::choice;
use super::segments::ItemKind;
use super::{BuiltinStatusLine, Segment};
use crate::theme::get_settings_list_theme;

pub(super) fn layout_values(builtin: &BuiltinStatusLine) -> Vec<Vec<Value>> {
    builtin
        .lines
        .iter()
        .map(|line| {
            line.iter()
                .map(|item| {
                    let mut value = match &item.kind {
                        ItemKind::Segment(segment) => json!({"segment": segment.name()}),
                        ItemKind::Text(text) => json!({"text": text}),
                    };
                    if let Some(color) = &item.color {
                        value["color"] = json!(color);
                    }
                    if let Some(icon) = &item.icon {
                        value["icon"] = json!(icon);
                    }
                    value
                })
                .collect()
        })
        .collect()
}

type Changes = Rc<RefCell<Vec<(String, String)>>>;

pub(super) struct LayoutEditor {
    lines: Vec<Vec<Value>>,
    list: SettingsList,
    changes: Changes,
    done: SubmenuDone,
}

impl LayoutEditor {
    pub(super) fn new(mut lines: Vec<Vec<Value>>, done: SubmenuDone) -> Self {
        if lines.is_empty() {
            lines.push(Vec::new());
        }
        let changes = Rc::default();
        let list = Self::list(&lines, &changes, &done);
        Self { lines, list, changes, done }
    }

    fn list(lines: &[Vec<Value>], changes: &Changes, done: &SubmenuDone) -> SettingsList {
        let mut items = Vec::new();
        for (row, line) in lines.iter().enumerate() {
            for (index, item) in line.iter().enumerate() {
                let name =
                    item.as_str().or_else(|| item["segment"].as_str()).or_else(|| item["text"].as_str()).unwrap_or("?");
                items.push(choice(
                    &format!("{row}:{index}"),
                    &format!("{}.{} {name}", row + 1, index + 1),
                    "Move within a row, move to another row, or remove this module.",
                    "edit",
                    &["earlier", "later", "previous row", "next row", "remove"],
                ));
            }
            let modules: Vec<&str> = Segment::ALL.iter().map(|segment| segment.name()).collect();
            items.push(choice(
                &format!("add:{row}"),
                &format!("Add to row {}", row + 1),
                "Fill separates left and right groups in spread alignment.",
                "choose",
                &modules,
            ));
        }
        for (id, label) in [("row", "Add row"), ("apply", "Apply layout")] {
            items.push(SettingItem {
                id: id.into(),
                label: label.into(),
                current_value: String::new(),
                values: Some(vec!["yes".into()]),
                ..Default::default()
            });
        }
        let changes = changes.clone();
        let done = done.clone();
        SettingsList::new(
            items,
            10,
            get_settings_list_theme(),
            move |id, value| changes.borrow_mut().push((id.into(), value.into())),
            move || done.done(None),
            SettingsListOptions::default(),
        )
    }

    fn apply_changes(&mut self) {
        let changes = std::mem::take(&mut *self.changes.borrow_mut());
        for (id, selected) in changes {
            if id == "apply" {
                self.done.done(Some(&json!(self.lines).to_string()));
                return;
            }
            change_layout(&mut self.lines, &id, &selected);
            self.list = Self::list(&self.lines, &self.changes, &self.done);
            self.list.select_item(&id);
        }
    }
}

pub(super) fn change_layout(lines: &mut Vec<Vec<Value>>, id: &str, selected: &str) {
    if id == "row" {
        lines.push(Vec::new());
        return;
    }
    let Some((row, index)) = id.split_once(':') else { return };
    if row == "add" {
        if let Ok(row) = index.parse::<usize>()
            && let Some(line) = lines.get_mut(row)
            && Segment::from_name(selected).is_some()
        {
            line.push(json!(selected));
        }
        return;
    }
    let (Ok(row), Ok(index)) = (row.parse::<usize>(), index.parse::<usize>()) else { return };
    let Some(line) = lines.get_mut(row).filter(|line| index < line.len()) else { return };
    match selected {
        "remove" => {
            line.remove(index);
        }
        "earlier" if index > 0 => line.swap(index, index - 1),
        "later" if index + 1 < line.len() => line.swap(index, index + 1),
        "previous row" if row > 0 => {
            let item = line.remove(index);
            lines[row - 1].push(item);
        }
        "next row" => {
            let item = line.remove(index);
            if row + 1 == lines.len() {
                lines.push(Vec::new());
            }
            lines[row + 1].push(item);
        }
        _ => {}
    }
}

impl Component for LayoutEditor {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.list.render(width)
    }
    fn handle_input(&mut self, data: &str) {
        self.list.handle_input(data);
        self.apply_changes();
    }
    fn handles_input(&self) -> bool {
        true
    }
    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<MouseResponse> {
        let response = self.list.handle_mouse(event);
        self.apply_changes();
        response
    }
    fn invalidate(&mut self) {
        self.list.invalidate();
    }
}
