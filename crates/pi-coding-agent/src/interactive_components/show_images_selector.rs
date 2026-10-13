//! [`ShowImagesSelector`]: yes/no choice for inline images (upstream `show-images-selector.ts`).

use std::cell::RefCell;
use std::rc::Rc;

use pi_tui::components::{SelectItem, SelectList, SelectListLayoutOptions};
use pi_tui::{Component, Container, component};

use super::DynamicBorder;
use crate::theme::get_select_list_theme;

fn layout() -> SelectListLayoutOptions {
    SelectListLayoutOptions {
        min_primary_column_width: Some(12),
        max_primary_column_width: Some(32),
        truncate_primary: None,
    }
}

/// A bordered "Yes"/"No" list preselecting the current value.
pub struct ShowImagesSelector {
    container: Container,
    select_list: Rc<RefCell<SelectList>>,
}

impl ShowImagesSelector {
    pub fn new(
        current_value: bool,
        mut on_select: impl FnMut(bool) + 'static,
        on_cancel: impl FnMut() + 'static,
    ) -> Self {
        let items = vec![
            SelectItem::new("yes", "Yes").with_description("Show images inline in terminal"),
            SelectItem::new("no", "No").with_description("Show text placeholder instead"),
        ];
        let mut list = SelectList::new(items, 5, get_select_list_theme(), layout());
        list.set_selected_index(if current_value { 0 } else { 1 });
        list.on_select = Some(Box::new(move |item: &SelectItem| on_select(item.value == "yes")));
        list.on_cancel = Some(Box::new(on_cancel));
        let select_list = component(list);
        let mut container = Container::new();
        container.add_child(component(DynamicBorder::new()));
        container.add_child(select_list.clone());
        container.add_child(component(DynamicBorder::new()));
        Self { container, select_list }
    }

    pub fn select_list(&self) -> Rc<RefCell<SelectList>> {
        self.select_list.clone()
    }
}

impl Component for ShowImagesSelector {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.container.render(width)
    }

    fn handle_input(&mut self, data: &str) {
        self.select_list.borrow_mut().handle_input(data);
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn invalidate(&mut self) {
        self.container.invalidate();
    }
}
