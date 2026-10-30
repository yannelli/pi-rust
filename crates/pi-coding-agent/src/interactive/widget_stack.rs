//! Extension widgets above and below the editor share a height budget (pi-rust addition; upstream
//! caps only text widgets). Over budget or collapsed, each widget shows its first line.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use pi_tui::utils::truncate_to_width_with;
use pi_tui::{Component, ComponentRef, strip_terminal_sequences, truncate_to_width, visible_width};

use super::extension_ui::UiMirror;
use crate::interactive_components::key_display_text;
use crate::theme::{ThemeColor, theme};

/// Widgets may fill a third of the terminal rows before they collapse.
const ROW_DIVISOR: usize = 3;
const MIN_BUDGET: usize = 3;
pub(super) const TOGGLE_KEY: &str = "app.widgets.toggle";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WidgetsView {
    /// Collapse only when the widgets need more rows than the budget.
    #[default]
    Auto,
    Collapsed,
    Expanded,
}

/// The widget rows for a terminal of `rows` rows; `None` while the size is unknown.
pub fn budget(rows: usize) -> Option<usize> {
    (rows > 0).then(|| (rows / ROW_DIVISOR).max(MIN_BUDGET))
}

fn dim(text: &str) -> String {
    theme().fg(ThemeColor::Dim, text)
}

/// The first line with text and a dim count of the hidden lines, cut to `width`.
fn summary(lines: &[String], width: usize, hint: Option<&str>) -> String {
    let first = lines.iter().find(|line| !strip_terminal_sequences(line).trim().is_empty()).unwrap_or(&lines[0]);
    let hidden = lines.len() - 1;
    let unit = if hidden == 1 { "line" } else { "lines" };
    let suffix = match hint {
        Some(hint) if !hint.is_empty() => dim(&format!(" +{hidden} {unit} · {hint}")),
        _ => dim(&format!(" +{hidden} {unit}")),
    };
    let room = width.saturating_sub(visible_width(&suffix));
    let content = visible_width(strip_terminal_sequences(first).trim_end());
    let head = if content <= room {
        truncate_to_width_with(first, content, "", false)
    } else {
        truncate_to_width(first, room)
    };
    truncate_to_width(&format!("{head}{suffix}"), width)
}

/// The lines each widget shows, from the rendered lines of every widget, top to bottom, and
/// whether they collapsed. Widgets past the budget give way to one `+N more widgets` line.
pub fn fit(
    widgets: Vec<Vec<String>>,
    view: WidgetsView,
    budget: Option<usize>,
    width: usize,
    hint: &str,
) -> (Vec<Vec<String>>, bool) {
    let total: usize = widgets.iter().map(Vec::len).sum();
    let collapse = match view {
        WidgetsView::Expanded => false,
        WidgetsView::Collapsed => true,
        WidgetsView::Auto => budget.is_some_and(|budget| total > budget),
    };
    if !collapse {
        return (widgets, false);
    }
    let visible = widgets.iter().filter(|lines| !lines.is_empty()).count();
    let limit = budget.filter(|budget| visible > *budget).map(|budget| budget.saturating_sub(1).max(1));
    let mut hint = Some(hint);
    let mut shown = 0;
    let mut fitted = Vec::with_capacity(widgets.len());
    for lines in widgets {
        if lines.is_empty() {
            fitted.push(lines);
            continue;
        }
        shown += 1;
        match limit {
            Some(limit) if shown > limit + 1 => fitted.push(Vec::new()),
            Some(limit) if shown == limit + 1 => {
                let more = format!(" +{} more widgets · /widgets", visible - limit);
                fitted.push(vec![truncate_to_width(&dim(&more), width)]);
            }
            _ if lines.len() == 1 => fitted.push(lines),
            _ => fitted.push(vec![summary(&lines, width, hint.take())]),
        }
    }
    (fitted, true)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Placement {
    Above,
    Below,
}

/// The mounted widgets of both placements and how they show.
pub(super) struct WidgetStack {
    pub above: Vec<(String, ComponentRef)>,
    pub below: Vec<(String, ComponentRef)>,
    pub view: WidgetsView,
    /// Whether the last render collapsed the widgets.
    pub collapsed: bool,
    mirror: Arc<UiMirror>,
    /// Lines of the widgets below the editor from the render above it, for the same width.
    below_lines: Option<(usize, Vec<String>)>,
}

pub(super) type SharedWidgetStack = Rc<RefCell<WidgetStack>>;

impl WidgetStack {
    pub fn shared(mirror: Arc<UiMirror>) -> SharedWidgetStack {
        let stack = Self {
            above: Vec::new(),
            below: Vec::new(),
            view: WidgetsView::Auto,
            collapsed: false,
            mirror,
            below_lines: None,
        };
        Rc::new(RefCell::new(stack))
    }

    fn rows(&self) -> usize {
        self.mirror.size.lock().map(|size| size.1 as usize).unwrap_or(0)
    }

    /// Every widget's own lines at `width`, keyed, above the editor first.
    pub fn render_all(&self, width: usize) -> Vec<(String, Vec<String>)> {
        let render = |(key, widget): &(String, ComponentRef)| {
            let lines = widget.try_borrow_mut().map(|mut widget| widget.render(width)).unwrap_or_default();
            (key.clone(), lines)
        };
        self.above.iter().chain(&self.below).map(render).collect()
    }

    /// The lines of both placements after [`fit`].
    fn layout(&mut self, width: usize) -> (Vec<String>, Vec<String>) {
        let widgets: Vec<Vec<String>> = self.render_all(width).into_iter().map(|(_, lines)| lines).collect();
        let hint = if self.view == WidgetsView::Auto { key_display_text(TOGGLE_KEY) } else { String::new() };
        let (fitted, collapsed) = fit(widgets, self.view, budget(self.rows()), width, &hint);
        self.collapsed = collapsed;
        let split = self.above.len();
        let below = fitted[split..].concat();
        (fitted[..split].concat(), below)
    }

    pub fn view_component(stack: &SharedWidgetStack, below: bool) -> ComponentRef {
        let placement = if below { Placement::Below } else { Placement::Above };
        pi_tui::component(WidgetStackView { stack: stack.clone(), placement })
    }
}

/// The widgets of one placement. The view above the editor renders first and keeps the lines
/// below it, so each widget renders once per frame.
struct WidgetStackView {
    stack: SharedWidgetStack,
    placement: Placement,
}

impl Component for WidgetStackView {
    fn render(&mut self, width: usize) -> Vec<String> {
        let Ok(mut stack) = self.stack.try_borrow_mut() else { return Vec::new() };
        if self.placement == Placement::Below
            && let Some((cached, lines)) = stack.below_lines.take()
            && cached == width
        {
            return lines;
        }
        let (above, below) = stack.layout(width);
        match self.placement {
            Placement::Above => {
                stack.below_lines = Some((width, below));
                above
            }
            Placement::Below => below,
        }
    }

    fn invalidate(&mut self) {
        let Ok(stack) = self.stack.try_borrow() else { return };
        for (_, widget) in stack.above.iter().chain(&stack.below) {
            if let Ok(mut widget) = widget.try_borrow_mut() {
                widget.invalidate();
            }
        }
    }
}

/// Lines as given, cut to the width: the `/widgets` view.
pub(super) struct StaticLines(pub Vec<String>);

impl Component for StaticLines {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.0.iter().map(|line| truncate_to_width(line, width)).collect()
    }
}

#[cfg(test)]
#[path = "widget_stack_tests.rs"]
mod tests;
