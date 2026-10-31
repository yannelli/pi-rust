//! [`ExtensionSelector`]: a titled list of string options for extensions and login prompts
//! (upstream `extension-selector.ts`).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use pi_tui::components::Text;
use pi_tui::{Component, Container, Tui, component, get_keybindings};

use super::common::{Shared, bold, border, fg, shared, spacer, text, visible_window};
use super::{CountdownTimer, key_hint, raw_key_hint};
use crate::theme::ThemeColor;

const MAX_VISIBLE: usize = 10;
/// Rows kept free for the footer under the selector.
const FOOTER_ROWS: usize = 4;

/// Optional behavior of an [`ExtensionSelector`].
#[derive(Default)]
pub struct ExtensionSelectorOptions {
    /// Needed for the countdown to re-render.
    pub tui: Option<Tui>,
    /// Cancels the selector when it runs out; shown as `(Ns)` after the title.
    pub timeout: Option<Duration>,
    /// Called on `app.tools.expand`.
    pub on_toggle_tools_expanded: Option<Box<dyn FnMut()>>,
    /// Text shown under the title.
    pub description: Option<String>,
}

pub struct ExtensionSelector {
    container: Container,
    options: Vec<String>,
    selected_index: usize,
    max_visible: usize,
    list: Rc<RefCell<Container>>,
    tui: Option<Tui>,
    on_select: Box<dyn FnMut(&str)>,
    on_cancel: Shared<dyn FnMut()>,
    on_toggle_tools_expanded: Option<Box<dyn FnMut()>>,
    countdown: Option<CountdownTimer>,
}

impl ExtensionSelector {
    pub fn new(
        title: &str,
        options: Vec<String>,
        on_select: impl FnMut(&str) + 'static,
        on_cancel: impl FnMut() + 'static,
        opts: ExtensionSelectorOptions,
    ) -> Self {
        let on_cancel: Shared<dyn FnMut()> = shared(Box::new(on_cancel));
        let mut container = Container::new();
        container.add_child(border());
        container.add_child(spacer(1));
        let title_text = component(Text::new(&fg(ThemeColor::Accent, &bold(title)), 1, 0));
        container.add_child(title_text.clone());
        if let Some(description) = &opts.description {
            container.add_child(spacer(1));
            container.add_child(text(&fg(ThemeColor::Text, description), 1, 0));
        }
        container.add_child(spacer(1));

        let countdown = match (opts.timeout, &opts.tui) {
            (Some(timeout), Some(tui)) if !timeout.is_zero() => {
                let base_title = title.to_string();
                let cancel = on_cancel.clone();
                Some(CountdownTimer::new(
                    timeout,
                    Some(tui.clone()),
                    move |seconds| {
                        let title = format!("{base_title} ({seconds}s)");
                        title_text.borrow_mut().set_text(&fg(ThemeColor::Accent, &bold(&title)));
                    },
                    move || (cancel.borrow_mut())(),
                ))
            }
            _ => None,
        };

        let list = component(Container::new());
        container.add_child(list.clone());
        container.add_child(spacer(1));
        let hint = format!(
            "{}  {}  {}",
            raw_key_hint("↑↓", "navigate"),
            key_hint("tui.select.confirm", "select"),
            key_hint("tui.select.cancel", "cancel")
        );
        container.add_child(text(&hint, 1, 0));
        container.add_child(spacer(1));
        container.add_child(border());

        let mut selector = Self {
            container,
            options,
            selected_index: 0,
            max_visible: MAX_VISIBLE,
            list,
            tui: opts.tui,
            on_select: Box::new(on_select),
            on_cancel,
            on_toggle_tools_expanded: opts.on_toggle_tools_expanded,
            countdown,
        };
        selector.update_list();
        selector
    }

    pub fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn update_list(&mut self) {
        let mut list = self.list.borrow_mut();
        list.clear();
        let (start, end) = visible_window(self.selected_index, self.options.len(), self.max_visible);
        for (index, option) in self.options.iter().enumerate().take(end).skip(start) {
            let line = if index == self.selected_index {
                fg(ThemeColor::Accent, "→ ") + &fg(ThemeColor::Accent, option)
            } else {
                format!("  {}", fg(ThemeColor::Text, option))
            };
            list.add_child(text(&line, 1, 0));
        }
        if end - start < self.options.len() {
            let count = format!("  ({}/{})", self.selected_index + 1, self.options.len());
            list.add_child(text(&fg(ThemeColor::Muted, &count), 1, 0));
        }
    }

    /// Stops the countdown.
    pub fn dispose(&mut self) {
        if let Some(countdown) = &self.countdown {
            countdown.dispose();
        }
    }

    /// Stops the countdown; usable from inside a callback.
    pub fn disposer(&self) -> impl Fn() + 'static {
        let countdown = self.countdown.as_ref().map(CountdownTimer::disposer);
        move || {
            if let Some(dispose) = &countdown {
                dispose();
            }
        }
    }
}

impl Component for ExtensionSelector {
    /// Shrinks the option window until the selector and footer fit the terminal height.
    fn render(&mut self, width: usize) -> Vec<String> {
        let rows = self.tui.as_ref().map(|tui| tui.terminal().borrow().rows());
        let mut max_visible = MAX_VISIBLE;
        loop {
            if self.max_visible != max_visible {
                self.max_visible = max_visible;
                self.update_list();
            }
            let lines = self.container.render(width);
            match rows {
                Some(rows) if max_visible > 1 && lines.len() + FOOTER_ROWS > rows => max_visible -= 1,
                _ => return lines,
            }
        }
    }

    fn handle_input(&mut self, data: &str) {
        let kb = get_keybindings();
        if kb.matches(data, "app.tools.expand") {
            if let Some(toggle) = self.on_toggle_tools_expanded.as_mut() {
                toggle();
            }
        } else if kb.matches(data, "tui.select.up") || data == "k" {
            self.selected_index = self.selected_index.saturating_sub(1);
            self.update_list();
        } else if kb.matches(data, "tui.select.down") || data == "j" {
            self.selected_index = (self.selected_index + 1).min(self.options.len().saturating_sub(1));
            self.update_list();
        } else if kb.matches(data, "tui.select.confirm") || data == "\n" {
            if let Some(selected) = self.options.get(self.selected_index).cloned() {
                (self.on_select)(&selected);
            }
        } else if kb.matches(data, "tui.select.cancel") {
            (self.on_cancel.borrow_mut())();
        }
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn invalidate(&mut self) {
        self.container.invalidate();
    }
}
