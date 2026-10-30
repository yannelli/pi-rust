//! `/widgets` and the `app.widgets.toggle` key: show every extension widget in full, or collapse
//! and expand the widgets around the editor.

use pi_tui::component;

use super::mode::InteractiveMode;
use super::widget_stack::{StaticLines, WidgetsView};
use crate::interactive_components::DynamicBorder;
use crate::theme::{ThemeColor, theme};

const USAGE: &str = "Usage: /widgets [collapse|expand|auto|toggle]";

impl InteractiveMode {
    /// Collapse the widgets when they show in full now, else expand them.
    pub(super) fn toggle_widgets(&self) {
        let stack = self.extensions_state().widget_stack();
        let showing_collapsed = {
            let stack = stack.borrow();
            stack.view == WidgetsView::Collapsed || (stack.view == WidgetsView::Auto && stack.collapsed)
        };
        self.set_widgets_view(if showing_collapsed { WidgetsView::Expanded } else { WidgetsView::Collapsed });
    }

    fn set_widgets_view(&self, view: WidgetsView) {
        self.extensions_state().widget_stack().borrow_mut().view = view;
        let label = match view {
            WidgetsView::Auto => "auto",
            WidgetsView::Collapsed => "collapsed",
            WidgetsView::Expanded => "expanded",
        };
        self.show_status(&format!("Widgets: {label}"));
    }

    pub(super) fn handle_widgets_command(&self, argument: Option<&str>) {
        match argument {
            None => self.show_all_widgets(),
            Some("toggle") => self.toggle_widgets(),
            Some("collapse") => self.set_widgets_view(WidgetsView::Collapsed),
            Some("expand") => self.set_widgets_view(WidgetsView::Expanded),
            Some("auto") => self.set_widgets_view(WidgetsView::Auto),
            Some(_) => self.show_warning(USAGE),
        }
    }

    /// Every widget in full in the chat, under its key.
    fn show_all_widgets(&self) {
        let width = self.terminal.borrow().columns().max(1);
        let stack = self.extensions_state().widget_stack();
        let widgets = stack.borrow().render_all(width);
        let mut lines = Vec::new();
        for (key, body) in widgets.into_iter().filter(|(_, body)| !body.is_empty()) {
            lines.push(format!(" {}", theme().bold(&theme().fg(ThemeColor::Accent, &key))));
            lines.extend(body);
        }
        if lines.is_empty() {
            return self.show_status("No extension widgets");
        }
        self.add_spacer();
        self.add_to_chat(component(DynamicBorder::new()));
        self.add_to_chat(component(StaticLines(lines)));
        self.add_to_chat(component(DynamicBorder::new()));
        self.request_render();
    }
}
