//! Text rebuilt from the active theme after every invalidation (upstream `themed-text.ts`), and
//! the expandable variant used by the startup header and loaded-resource sections.

use std::cell::Cell;
use std::rc::Rc;

use pi_tui::{Component, MouseResponse, Text, TuiMouseEvent, TuiMouseEventResult, TuiMouseEventType};

/// Builds the text; must return the same content each time apart from colors.
pub type TextBuilder = Box<dyn Fn() -> String>;

/// Text whose content applies theme colors. A theme change invalidates the UI, which rebuilds it.
pub struct ThemedText {
    text: Text,
    build: TextBuilder,
    stale: bool,
}

impl ThemedText {
    pub fn new(build: impl Fn() -> String + 'static, padding_x: usize, padding_y: usize) -> Self {
        Self { text: Text::new("", padding_x, padding_y), build: Box::new(build), stale: true }
    }
}

impl Component for ThemedText {
    fn render(&mut self, width: usize) -> Vec<String> {
        if self.stale {
            self.stale = false;
            self.text.set_text(&(self.build)());
        }
        self.text.render(width)
    }

    fn invalidate(&mut self) {
        self.text.invalidate();
        self.stale = true;
    }
}

/// Logo clicks of the built-in header: `(column, row)` of the logo's top-left cell.
pub type LogoClick = Box<dyn FnMut(i32, i32)>;

/// Themed text with a collapsed and an expanded form, toggled by `Ctrl+O` (upstream `ExpandableText`).
pub struct ExpandableText {
    inner: ThemedText,
    expanded: Rc<Cell<bool>>,
    on_logo_click: Option<LogoClick>,
}

impl ExpandableText {
    pub fn new(
        collapsed: impl Fn() -> String + 'static,
        expanded_text: impl Fn() -> String + 'static,
        expanded: bool,
        padding_x: usize,
        padding_y: usize,
    ) -> Self {
        let state = Rc::new(Cell::new(expanded));
        let flag = state.clone();
        let build = move || if flag.get() { expanded_text() } else { collapsed() };
        Self { inner: ThemedText::new(build, padding_x, padding_y), expanded: state, on_logo_click: None }
    }

    pub fn is_expanded(&self) -> bool {
        self.expanded.get()
    }

    pub fn set_expanded(&mut self, expanded: bool) {
        self.expanded.set(expanded);
        self.inner.invalidate();
    }

    /// Makes the first two lines after one column of padding a clickable logo (built-in header).
    pub fn set_on_logo_click(&mut self, on_click: Option<LogoClick>) {
        self.on_logo_click = on_click;
    }
}

impl Component for ExpandableText {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.inner.render(width)
    }

    fn invalidate(&mut self) {
        self.inner.invalidate();
    }

    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<MouseResponse> {
        let on_click = self.on_logo_click.as_mut()?;
        if event.kind != TuiMouseEventType::Click || event.y > 1 || event.x < 1 || event.x > 4 {
            return None;
        }
        on_click(event.screen_x - event.x + 1, event.screen_y - event.y);
        Some(MouseResponse::Handled(TuiMouseEventResult::handled()))
    }
}
