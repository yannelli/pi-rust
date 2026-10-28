//! Text rebuilt from the active theme after every invalidation (upstream `themed-text.ts`), and
//! the expandable variant used by the startup header and loaded-resource sections.

use std::cell::Cell;
use std::rc::Rc;

use pi_tui::{Component, MouseResponse, Text, TuiMouseEvent, TuiMouseEventResult, TuiMouseEventType};

use super::pi_logo::{PI_LOGO_HEIGHT, PI_LOGO_WIDTH};

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

    /// Makes the logo's cells at the top left, after one column of padding, clickable (built-in header).
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
        let (width, height) = (PI_LOGO_WIDTH as i32, PI_LOGO_HEIGHT as i32);
        if event.kind != TuiMouseEventType::Click || event.y >= height || event.x < 1 || event.x > width {
            return None;
        }
        on_click(event.screen_x - event.x + 1, event.screen_y - event.y);
        Some(MouseResponse::Handled(TuiMouseEventResult::handled()))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use pi_tui::{Component, TuiMouseButton, TuiMouseEvent, TuiMouseEventType};

    use super::ExpandableText;

    fn click(x: i32, y: i32) -> TuiMouseEvent {
        TuiMouseEvent {
            kind: TuiMouseEventType::Click,
            button: TuiMouseButton::Left,
            x,
            y,
            screen_x: x + 10,
            screen_y: y + 5,
            width: 80,
            height: 24,
            shift: false,
            alt: false,
            ctrl: false,
            wheel_delta: None,
            click_count: Some(1),
        }
    }

    #[test]
    fn logo_clicks_cover_the_logo_and_report_its_top_left_cell() {
        let clicks = Rc::new(RefCell::new(Vec::new()));
        let record = clicks.clone();
        let mut header = ExpandableText::new(String::new, String::new, false, 1, 0);
        header.set_on_logo_click(Some(Box::new(move |column, row| record.borrow_mut().push((column, row)))));
        for (x, y) in [(1, 0), (4, 1), (0, 0), (5, 0), (1, 2)] {
            header.handle_mouse(&click(x, y));
        }
        assert_eq!(*clicks.borrow(), [(11, 5), (11, 5)]);
    }
}
