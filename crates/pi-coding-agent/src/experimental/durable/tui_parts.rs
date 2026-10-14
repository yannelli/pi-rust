//! Parts of upstream `durable/tui.ts`: the filterable list selector, the compaction card, and the plain
//! helpers that read the view's task graph, usage, and context size.

use std::cell::RefCell;
use std::rc::Rc;

use pi_ai::{Message, StopReason, UserContent};
use pi_durable::EntryRecord;
use pi_tui::components::markdown::{DefaultTextStyle, MarkdownOptions};
use pi_tui::components::select_list::{SelectItem, SelectListLayoutOptions};
use pi_tui::components::{BoxComponent, Input, Markdown, SelectList, Spacer, Text};
use pi_tui::fuzzy::fuzzy_filter;
use pi_tui::{Component, ComponentRef, Container, component, get_keybindings};
use serde_json::Value;

use crate::interactive_components::DynamicBorder;
use crate::interactive_components::keybinding_hints::key_text;
use crate::theme::{ThemeBg, ThemeColor, theme};
use crate::theme::{get_markdown_theme, get_select_list_theme};

pub type SelectFn = Rc<dyn Fn(String)>;
pub type CancelFn = Rc<dyn Fn()>;

/// A filterable list in place of the editor.
pub struct ListSelector {
    root: Container,
    input: Rc<RefCell<Input>>,
    list_slot: Rc<RefCell<Container>>,
    list: Rc<RefCell<SelectList>>,
    items: Vec<SelectItem>,
    on_select: SelectFn,
    on_cancel: CancelFn,
}

impl ListSelector {
    pub fn new(title: &str, items: Vec<SelectItem>, on_select: SelectFn, on_cancel: CancelFn) -> Self {
        let input = component(Input::default());
        let list_slot = component(Container::new());
        let list = build_list(&list_slot, items.clone(), &on_select, &on_cancel);
        let mut root = Container::new();
        root.add_child(component(DynamicBorder::new()));
        root.add_child(component(Spacer::new(1)));
        root.add_child(component(Text::new(&theme().fg(ThemeColor::Accent, &theme().bold(title)), 1, 0)));
        root.add_child(input.clone());
        root.add_child(component(Spacer::new(1)));
        root.add_child(list_slot.clone());
        root.add_child(component(DynamicBorder::new()));
        Self { root, input, list_slot, list, items, on_select, on_cancel }
    }
}

fn build_list(
    slot: &Rc<RefCell<Container>>,
    items: Vec<SelectItem>,
    on_select: &SelectFn,
    on_cancel: &CancelFn,
) -> Rc<RefCell<SelectList>> {
    let mut list = SelectList::new(items, 10, get_select_list_theme(), SelectListLayoutOptions::default());
    let (select, cancel) = (on_select.clone(), on_cancel.clone());
    list.on_select = Some(Box::new(move |item: &SelectItem| select(item.value.clone())));
    list.on_cancel = Some(Box::new(move || cancel()));
    let list = component(list);
    let mut slot = slot.borrow_mut();
    slot.clear();
    slot.add_child(list.clone());
    list
}

impl Component for ListSelector {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.root.render(width)
    }

    fn handle_input(&mut self, data: &str) {
        let keybindings = get_keybindings();
        let forwarded = ["tui.select.up", "tui.select.down", "tui.select.confirm", "tui.select.cancel"];
        if forwarded.iter().any(|action| keybindings.matches(data, action)) {
            self.list.borrow_mut().handle_input(data);
            return;
        }
        self.input.borrow_mut().handle_input(data);
        let query = self.input.borrow().get_value().to_string();
        let filtered = match query.is_empty() {
            true => self.items.clone(),
            false => {
                let keyed: Vec<(String, SelectItem)> =
                    self.items.iter().map(|item| (format!("{} {}", item.label, item.value), item.clone())).collect();
                fuzzy_filter(keyed, &query, |(key, _)| key.as_str()).into_iter().map(|(_, item)| item).collect()
            }
        };
        self.list = build_list(&self.list_slot, filtered, &self.on_select, &self.on_cancel);
    }

    fn invalidate(&mut self) {
        self.root.invalidate();
    }

    fn is_focusable(&self) -> bool {
        true
    }

    fn set_focused(&mut self, focused: bool) {
        self.input.borrow_mut().set_focused(focused);
    }

    fn container_children(&self) -> Option<Vec<ComponentRef>> {
        self.root.container_children()
    }
}

/// The summary that replaced earlier context: collapsed to one line until expanded.
pub struct CompactionCard {
    pub component: Rc<RefCell<BoxComponent>>,
    summary: String,
}

impl CompactionCard {
    pub fn new(summary: String, expanded: bool) -> Self {
        let background: pi_tui::components::StyleFn = Rc::new(|text: &str| theme().bg(ThemeBg::CustomMessageBg, text));
        let card = Self { component: component(BoxComponent::new(1, 1, Some(background))), summary };
        card.set_expanded(expanded);
        card
    }

    pub fn set_expanded(&self, expanded: bool) {
        let mut card = self.component.borrow_mut();
        card.clear();
        let label = theme().fg(ThemeColor::CustomMessageLabel, &theme().bold("[compaction]"));
        card.add_child(component(Text::new(&label, 0, 0)));
        card.add_child(component(Spacer::new(1)));
        if expanded {
            let style = DefaultTextStyle {
                color: Some(Rc::new(|text: &str| theme().fg(ThemeColor::CustomMessageText, text))),
                ..DefaultTextStyle::default()
            };
            let markdown =
                Markdown::new(&self.summary, 0, 0, get_markdown_theme(), Some(style), MarkdownOptions::default());
            card.add_child(component(markdown));
        } else {
            let text = theme().fg(ThemeColor::CustomMessageText, "Earlier context summarized (")
                + &theme().fg(ThemeColor::Dim, &key_text("app.tools.expand"))
                + &theme().fg(ThemeColor::CustomMessageText, " to expand)");
            card.add_child(component(Text::new(&text, 0, 0)));
        }
    }
}

/// One line of the task panel for a task graph node.
pub fn describe_task(node: &Value) -> String {
    let state = &node["state"];
    let text = |value: &Value| match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    let status = match state["status"].as_str().unwrap_or_default() {
        "waiting" => {
            let on: Vec<String> = state["on"].as_array().into_iter().flatten().map(text).collect();
            format!("waiting on {}", on.join(", "))
        }
        "completing" => format!("completing ({})", text(&state["outcome"])),
        status => format!("{status} {}", text(&state["phase"])),
    };
    let mut flags = Vec::new();
    if node["background"] == true {
        flags.push("background");
    }
    if node["abortRequested"] == true {
        flags.push("aborting");
    }
    let conversations: Vec<String> = node["conversations"].as_array().into_iter().flatten().map(text).collect();
    let owned = match conversations.is_empty() {
        true => String::new(),
        false => format!(" owns conversation {}", conversations.join(", ")),
    };
    let flags = if flags.is_empty() { String::new() } else { format!(" [{}]", flags.join(", ")) };
    format!("{} #{}: {status}{flags}{owned}", text(&node["kind"]), text(&node["id"]))
}

pub fn user_text(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                pi_ai::ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect(),
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct UsageTotal {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: f64,
}

/// The usage of every model and tool in a `pi.usage` document.
pub fn total_usage(state: &Value) -> UsageTotal {
    let mut total = UsageTotal::default();
    for group in ["models", "tools"] {
        for usage in state[group].as_object().into_iter().flat_map(|map| map.values()) {
            let count = |key: &str| usage[key].as_u64().unwrap_or_default();
            total.input += count("input");
            total.output += count("output");
            total.cache_read += count("cacheRead");
            total.cache_write += count("cacheWrite");
            total.cost += usage["cost"]["total"].as_f64().unwrap_or_default();
        }
    }
    total
}

/// Context size from the newest successful answer after the newest compaction; unknown before one.
pub fn context_tokens(entries: &[EntryRecord]) -> Option<u64> {
    // Kept entries follow the summary in the view but are older than it; only later answers measure it.
    let compacted = entries.iter().filter(|entry| entry.kind == "pi.compaction").map(|entry| entry.id).max();
    for entry in entries.iter().rev() {
        if compacted.is_some_and(|compacted| entry.id < compacted) || entry.kind != "pi.assistant" {
            continue;
        }
        let Some(Message::Assistant(message)) = entry.model.as_ref().and_then(|model| model.first()) else {
            continue;
        };
        if matches!(message.stop_reason, StopReason::Aborted | StopReason::Error) {
            continue;
        }
        let usage = &message.usage;
        let total = usage.total_tokens;
        return Some(if total > 0 { total } else { usage.input + usage.output + usage.cache_read + usage.cache_write });
    }
    None
}
