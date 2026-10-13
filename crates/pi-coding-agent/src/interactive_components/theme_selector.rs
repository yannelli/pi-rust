//! [`ThemeSelector`]: pick a theme with live preview (upstream `theme-selector.ts`).

use std::cell::RefCell;
use std::rc::Rc;

use pi_tui::components::{SelectItem, SelectList, SelectListLayoutOptions};
use pi_tui::{Component, Container, component};

use super::DynamicBorder;
use crate::theme::{get_available_themes, get_select_list_theme};

fn layout() -> SelectListLayoutOptions {
    SelectListLayoutOptions {
        min_primary_column_width: Some(12),
        max_primary_column_width: Some(32),
        truncate_primary: None,
    }
}

/// A bordered list of the available themes. Moving the selection calls `on_preview`, which hosts
/// route to [`crate::theme::InteractiveThemeController::preview`].
pub struct ThemeSelector {
    container: Container,
    select_list: Rc<RefCell<SelectList>>,
}

impl ThemeSelector {
    pub fn new(
        current_theme: &str,
        mut on_select: impl FnMut(&str) + 'static,
        on_cancel: impl FnMut() + 'static,
        mut on_preview: impl FnMut(&str) + 'static,
    ) -> Self {
        Self::with_themes(
            get_available_themes(),
            current_theme,
            move |name| on_select(name),
            on_cancel,
            move |name| on_preview(name),
        )
    }

    /// Like [`Self::new`] with an explicit theme list.
    pub fn with_themes(
        themes: Vec<String>,
        current_theme: &str,
        mut on_select: impl FnMut(&str) + 'static,
        on_cancel: impl FnMut() + 'static,
        mut on_preview: impl FnMut(&str) + 'static,
    ) -> Self {
        let items = themes
            .iter()
            .map(|name| {
                let item = SelectItem::new(name, name);
                if name == current_theme { item.with_description("(current)") } else { item }
            })
            .collect();
        let mut list = SelectList::new(items, 10, get_select_list_theme(), layout());
        if let Some(index) = themes.iter().position(|name| name == current_theme) {
            list.set_selected_index(index);
        }
        list.on_select = Some(Box::new(move |item: &SelectItem| on_select(&item.value)));
        list.on_cancel = Some(Box::new(on_cancel));
        list.on_selection_change = Some(Box::new(move |item: &SelectItem| on_preview(&item.value)));
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

impl Component for ThemeSelector {
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
