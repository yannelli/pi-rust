//! [`ThinkingSelector`]: pick a thinking level, with search and save-as-default
//! (upstream `thinking-selector.ts`).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use pi_ai::ThinkingLevel;
use pi_tui::components::{Input, SelectItem, SelectList, SelectListLayoutOptions};
use pi_tui::{Component, ComponentRef, Container, get_keybindings};

use super::common::{Shared, border, fg, fuzzy_filter_by, shared, spacer, text};
use super::key_display_text;
use crate::theme::{ThemeColor, get_select_list_theme};

fn level_description(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off => "No reasoning",
        ThinkingLevel::Minimal => "Very brief reasoning (~1k tokens)",
        ThinkingLevel::Low => "Light reasoning (~2k tokens)",
        ThinkingLevel::Medium => "Moderate reasoning (~8k tokens)",
        ThinkingLevel::High => "Deep reasoning (~16k tokens)",
        ThinkingLevel::Xhigh => "Extra-high reasoning (~32k tokens)",
        ThinkingLevel::Max => "Maximum reasoning",
    }
}

fn parse_level(value: &str) -> ThinkingLevel {
    value.parse().unwrap_or_default()
}

/// Thinking level selector. Typing filters the levels; `app.thinking.save` sets the selected
/// level as the default when `on_select_as_default` is given.
pub struct ThinkingSelector {
    container: Container,
    search_input: Rc<RefCell<Input>>,
    select_list: Rc<RefCell<SelectList>>,
    list_index: usize,
    all_items: Vec<SelectItem>,
    on_select: Shared<dyn FnMut(ThinkingLevel)>,
    on_cancel: Shared<dyn FnMut()>,
    on_select_as_default: Option<Box<dyn FnMut(ThinkingLevel)>>,
    submitted: Rc<Cell<bool>>,
}

impl ThinkingSelector {
    pub fn new(
        current_level: ThinkingLevel,
        available_levels: &[ThinkingLevel],
        on_select: impl FnMut(ThinkingLevel) + 'static,
        on_cancel: impl FnMut() + 'static,
        on_select_as_default: Option<Box<dyn FnMut(ThinkingLevel)>>,
        default_thinking_level: Option<ThinkingLevel>,
    ) -> Self {
        let all_items: Vec<SelectItem> = available_levels
            .iter()
            .map(|&level| {
                let marker = if level == current_level { "✓ " } else { "  " };
                let description = level_description(level);
                let description = if Some(level) == default_thinking_level {
                    format!("{description} · default")
                } else {
                    description.to_string()
                };
                SelectItem::new(level.as_str(), &format!("{marker}{}", level.as_str())).with_description(&description)
            })
            .collect();
        let on_select: Shared<dyn FnMut(ThinkingLevel)> = shared(Box::new(on_select));
        let on_cancel: Shared<dyn FnMut()> = shared(Box::new(on_cancel));

        let mut container = Container::new();
        container.add_child(border());
        container.add_child(spacer(1));
        container.add_child(text("Thinking Level", 0, 0));
        container.add_child(spacer(1));
        let cycle = key_display_text("app.thinking.cycle");
        container.add_child(text(&format!("{cycle} cycles thinking levels in-session"), 0, 0));
        container.add_child(spacer(1));

        let submitted = Rc::new(Cell::new(false));
        let mut input = Input::default();
        let flag = submitted.clone();
        input.set_on_submit(move |_| flag.set(true));
        let search_input = Rc::new(RefCell::new(input));
        container.add_child(search_input.clone());
        container.add_child(spacer(1));

        let select_list = build_list(&all_items, Some(current_level.as_str()), &on_select, &on_cancel);
        let list_index = container.children.len();
        container.add_child(select_list.clone());
        container.add_child(spacer(1));
        let hint = format!(
            "  {} to select · {} to set as default · {} to cancel",
            key_display_text("tui.select.confirm"),
            key_display_text("app.thinking.save"),
            key_display_text("tui.select.cancel"),
        );
        container.add_child(text(&fg(ThemeColor::Dim, &hint), 0, 0));
        container.add_child(border());

        Self {
            container,
            search_input,
            select_list,
            list_index,
            all_items,
            on_select,
            on_cancel,
            on_select_as_default,
            submitted,
        }
    }

    pub fn select_list(&self) -> Rc<RefCell<SelectList>> {
        self.select_list.clone()
    }

    pub fn search_input(&self) -> Rc<RefCell<Input>> {
        self.search_input.clone()
    }

    fn apply_filter(&mut self, query: &str) {
        let filtered = if query.is_empty() {
            self.all_items.clone()
        } else {
            fuzzy_filter_by(&self.all_items, query, |item: &SelectItem| {
                format!("{} {}", item.value, item.description.as_deref().unwrap_or(""))
            })
        };
        let selected = self.select_list.borrow().get_selected_item().map(|item| item.value.clone());
        let list = build_list(&filtered, selected.as_deref(), &self.on_select, &self.on_cancel);
        let erased: ComponentRef = list.clone();
        self.container.children[self.list_index] = erased;
        self.select_list = list;
    }
}

fn build_list(
    items: &[SelectItem],
    preselect: Option<&str>,
    on_select: &Shared<dyn FnMut(ThinkingLevel)>,
    on_cancel: &Shared<dyn FnMut()>,
) -> Rc<RefCell<SelectList>> {
    let layout = SelectListLayoutOptions {
        min_primary_column_width: Some(12),
        max_primary_column_width: Some(32),
        truncate_primary: None,
    };
    let mut list = SelectList::new(items.to_vec(), items.len().max(1), get_select_list_theme(), layout);
    if let Some(index) = items.iter().position(|item| Some(item.value.as_str()) == preselect) {
        list.set_selected_index(index);
    }
    let on_select = on_select.clone();
    list.on_select = Some(Box::new(move |item: &SelectItem| (on_select.borrow_mut())(parse_level(&item.value))));
    let on_cancel = on_cancel.clone();
    list.on_cancel = Some(Box::new(move || (on_cancel.borrow_mut())()));
    Rc::new(RefCell::new(list))
}

impl Component for ThinkingSelector {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.container.render(width)
    }

    fn handle_input(&mut self, data: &str) {
        let kb = get_keybindings();
        if kb.matches(data, "app.thinking.save")
            && let Some(on_select_as_default) = self.on_select_as_default.as_mut()
        {
            let selected = self.select_list.borrow().get_selected_item().map(|item| parse_level(&item.value));
            if let Some(level) = selected {
                on_select_as_default(level);
            }
            return;
        }
        let is_nav = ["tui.select.up", "tui.select.down", "tui.select.confirm", "tui.select.cancel"]
            .iter()
            .any(|id| kb.matches(data, id));
        if is_nav {
            self.select_list.borrow_mut().handle_input(data);
            return;
        }
        self.search_input.borrow_mut().handle_input(data);
        if self.submitted.replace(false) {
            self.select_list.borrow_mut().handle_input("\r");
        }
        let query = self.search_input.borrow().get_value().to_string();
        self.apply_filter(&query);
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn is_focusable(&self) -> bool {
        true
    }

    fn set_focused(&mut self, focused: bool) {
        self.search_input.borrow_mut().focused = focused;
    }

    fn invalidate(&mut self) {
        self.container.invalidate();
    }
}
