//! [`UserMessageSelector`]: pick a user message to fork from (upstream `user-message-selector.ts`).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use pi_tui::utils::truncate_to_width;
use pi_tui::{Component, Container, component, get_keybindings, get_select_page_index, ui_loop};

use super::common::{Shared, bold, border, fg, shared, spacer, text};
use crate::theme::ThemeColor;

/// One user message of the active branch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UserMessageItem {
    /// Entry id in the session.
    pub id: String,
    /// The message text.
    pub text: String,
    pub timestamp: Option<String>,
}

impl UserMessageItem {
    pub fn new(id: &str, text: &str) -> Self {
        Self { id: id.to_string(), text: text.to_string(), timestamp: None }
    }
}

/// The message list: two lines per message plus a blank line, wrapping navigation.
pub struct UserMessageList {
    messages: Vec<UserMessageItem>,
    selected_index: usize,
    max_visible: usize,
    pub on_select: Option<Box<dyn FnMut(String)>>,
    pub on_cancel: Option<Box<dyn FnMut()>>,
}

impl UserMessageList {
    /// Messages in chronological order; selects `initial_selected_id` or the newest message.
    pub fn new(messages: Vec<UserMessageItem>, initial_selected_id: Option<&str>) -> Self {
        let initial = initial_selected_id.and_then(|id| messages.iter().position(|message| message.id == id));
        let selected_index = initial.unwrap_or(messages.len().saturating_sub(1));
        Self { messages, selected_index, max_visible: 10, on_select: None, on_cancel: None }
    }

    pub fn selected_index(&self) -> usize {
        self.selected_index
    }
}

impl Component for UserMessageList {
    fn render(&mut self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        if self.messages.is_empty() {
            lines.push(fg(ThemeColor::Muted, "  No user messages found"));
            return lines;
        }
        let len = self.messages.len();
        let start = (self.selected_index as i64 - (self.max_visible / 2) as i64)
            .min(len as i64 - self.max_visible as i64)
            .max(0) as usize;
        let end = (start + self.max_visible).min(len);
        for index in start..end {
            let message = &self.messages[index];
            let selected = index == self.selected_index;
            let normalized = message.text.replace('\n', " ");
            let truncated = truncate_to_width(normalized.trim(), width.saturating_sub(2));
            let cursor = if selected { fg(ThemeColor::Accent, "› ") } else { "  ".to_string() };
            lines.push(cursor + &if selected { bold(&truncated) } else { truncated });
            lines.push(fg(ThemeColor::Muted, &format!("  Message {} of {len}", index + 1)));
            lines.push(String::new());
        }
        if start > 0 || end < len {
            lines.push(fg(ThemeColor::Muted, &format!("  ({}/{len})", self.selected_index + 1)));
        }
        lines
    }

    fn handle_input(&mut self, data: &str) {
        let kb = get_keybindings();
        let last = self.messages.len().saturating_sub(1);
        let page_index = get_select_page_index(&kb, data, self.selected_index, self.messages.len(), self.max_visible);
        if kb.matches(data, "tui.select.up") {
            self.selected_index = if self.selected_index == 0 { last } else { self.selected_index - 1 };
        } else if kb.matches(data, "tui.select.down") {
            self.selected_index = if self.selected_index >= last { 0 } else { self.selected_index + 1 };
        } else if let Some(page_index) = page_index {
            if !self.messages.is_empty() {
                self.selected_index = page_index;
            }
        } else if kb.matches(data, "tui.select.confirm") {
            let selected = self.messages.get(self.selected_index).map(|message| message.id.clone());
            if let (Some(id), Some(on_select)) = (selected, self.on_select.as_mut()) {
                on_select(id);
            }
        } else if kb.matches(data, "tui.select.cancel")
            && let Some(on_cancel) = self.on_cancel.as_mut()
        {
            on_cancel();
        }
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn captures_viewport_input(&self) -> bool {
        true
    }
}

/// "Fork from Message" dialog. Input goes to the message list; an empty list cancels after
/// 100 ms, as upstream does.
pub struct UserMessageSelector {
    container: Container,
    message_list: Rc<RefCell<UserMessageList>>,
}

impl UserMessageSelector {
    pub fn new(
        messages: Vec<UserMessageItem>,
        on_select: impl FnMut(String) + 'static,
        on_cancel: impl FnMut() + 'static,
        initial_selected_id: Option<&str>,
    ) -> Self {
        let mut container = Container::new();
        container.add_child(spacer(1));
        container.add_child(text(&bold("Fork from Message"), 1, 0));
        let description = "Select a user message to copy the active path up to that point into a new session";
        container.add_child(text(&fg(ThemeColor::Muted, description), 1, 0));
        container.add_child(spacer(1));
        container.add_child(border());
        container.add_child(spacer(1));

        let empty = messages.is_empty();
        let on_cancel: Shared<dyn FnMut()> = shared(Box::new(on_cancel));
        let mut list = UserMessageList::new(messages, initial_selected_id);
        list.on_select = Some(Box::new(on_select));
        let list_cancel = on_cancel.clone();
        list.on_cancel = Some(Box::new(move || (list_cancel.borrow_mut())()));
        let message_list = component(list);
        container.add_child(message_list.clone());
        container.add_child(spacer(1));
        container.add_child(border());

        if empty {
            ui_loop::set_timeout(Duration::from_millis(100), move || (on_cancel.borrow_mut())());
        }
        Self { container, message_list }
    }

    pub fn message_list(&self) -> Rc<RefCell<UserMessageList>> {
        self.message_list.clone()
    }
}

impl Component for UserMessageSelector {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.container.render(width)
    }

    fn handle_input(&mut self, data: &str) {
        self.message_list.borrow_mut().handle_input(data);
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn captures_viewport_input(&self) -> bool {
        self.message_list.borrow().captures_viewport_input()
    }

    fn invalidate(&mut self) {
        self.container.invalidate();
    }
}
