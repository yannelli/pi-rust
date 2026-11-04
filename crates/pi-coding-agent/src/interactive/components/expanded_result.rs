//! The expanded body of a tool result that an extension draws (`registerToolResultExpander`):
//! at most `max_height` lines, scrolled with the mouse wheel in fullscreen mode.

use std::cell::Cell;
use std::rc::Rc;

use pi_tui::{
    Component, ComponentRef, MouseResponse, TuiMouseEvent, TuiMouseEventResult, TuiMouseEventType, dispatch_mouse_event,
};

use crate::theme::{ThemeColor, theme};

pub struct ExpandedResult {
    child: ComponentRef,
    /// Lines in view, the position line included; 0 shows every line.
    max_height: usize,
    /// First line in view, kept by the tool row across rebuilds.
    offset: Rc<Cell<usize>>,
    total: Cell<usize>,
    wheel: bool,
}

impl ExpandedResult {
    /// `wheel`: the terminal reports the mouse wheel (fullscreen mode).
    pub fn new(child: ComponentRef, max_height: usize, offset: Rc<Cell<usize>>, wheel: bool) -> Self {
        Self { child, max_height, offset, total: Cell::new(0), wheel }
    }

    fn rows(&self) -> usize {
        self.max_height.saturating_sub(1).max(1)
    }

    /// Moves the view by `delta` lines; false when it is already at that edge.
    pub fn scroll_by(&self, delta: i64) -> bool {
        let last = self.total.get().saturating_sub(self.rows());
        let current = self.offset.get();
        let next = (current as i64 + delta).clamp(0, last as i64) as usize;
        self.offset.set(next);
        next != current
    }
}

impl Component for ExpandedResult {
    fn render(&mut self, width: usize) -> Vec<String> {
        let lines = self.child.borrow_mut().render(width);
        self.total.set(lines.len());
        if self.max_height == 0 || lines.len() <= self.max_height {
            return lines;
        }
        let rows = self.rows();
        let start = self.offset.get().min(lines.len() - rows);
        self.offset.set(start);
        let mut shown = lines[start..start + rows].to_vec();
        let scroll = if self.wheel { " · scroll with the mouse wheel" } else { "" };
        let position = format!("lines {}-{} of {}{scroll}", start + 1, start + rows, lines.len());
        shown.push(theme().fg(ThemeColor::Muted, &pi_tui::truncate_to_width(&position, width)));
        shown
    }

    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<MouseResponse> {
        if event.kind == TuiMouseEventType::Wheel {
            let delta = event.wheel_delta.unwrap_or(0) as i64;
            if self.max_height > 0 && self.total.get() > self.max_height && self.scroll_by(delta) {
                return Some(MouseResponse::Handled(TuiMouseEventResult::handled()));
            }
            return None;
        }
        dispatch_mouse_event(&self.child, event).map(MouseResponse::Forwarded)
    }

    fn invalidate(&mut self) {
        self.child.borrow_mut().invalidate();
    }
}

#[cfg(test)]
mod tests {
    use pi_tui::utils::strip_terminal_sequences;
    use pi_tui::{Text, component};

    use super::*;

    fn view(lines: usize, max: usize, wheel: bool) -> (ExpandedResult, Rc<Cell<usize>>) {
        let text: Vec<String> = (1..=lines).map(|n| format!("line {n}")).collect();
        let offset = Rc::new(Cell::new(0));
        let child = component(Text::new(&text.join("\n"), 0, 0));
        (ExpandedResult::new(child, max, offset.clone(), wheel), offset)
    }

    fn plain(lines: Vec<String>) -> Vec<String> {
        lines.iter().map(|line| strip_terminal_sequences(line).trim_end().to_string()).collect()
    }

    fn wheel(delta: i32) -> TuiMouseEvent {
        TuiMouseEvent {
            kind: TuiMouseEventType::Wheel,
            button: pi_tui::TuiMouseButton::None,
            x: 0,
            y: 0,
            screen_x: 0,
            screen_y: 0,
            width: 40,
            height: 5,
            shift: false,
            alt: false,
            ctrl: false,
            wheel_delta: Some(delta),
            click_count: None,
        }
    }

    #[test]
    fn limits_the_height_and_scrolls_with_the_wheel() {
        let (mut view, offset) = view(20, 5, true);
        let first = plain(view.render(60));
        assert_eq!(first, ["line 1", "line 2", "line 3", "line 4", "lines 1-4 of 20 · scroll with the mouse wheel"]);
        assert!(view.handle_mouse(&wheel(3)).is_some());
        assert_eq!(offset.get(), 3);
        assert_eq!(plain(view.render(40))[0], "line 4");
        assert!(view.handle_mouse(&wheel(100)).is_some());
        let end = plain(view.render(40));
        assert_eq!(end[..4], ["line 17", "line 18", "line 19", "line 20"]);
        assert!(view.handle_mouse(&wheel(1)).is_none(), "the edge passes the wheel on");
        assert!(view.handle_mouse(&wheel(-100)).is_some());
        assert_eq!(offset.get(), 0);
    }

    #[test]
    fn short_bodies_and_no_limit_show_every_line() {
        let (mut short, _) = view(3, 5, true);
        assert_eq!(plain(short.render(40)), ["line 1", "line 2", "line 3"]);
        assert!(short.handle_mouse(&wheel(1)).is_none());
        let (mut open, _) = view(30, 0, true);
        assert_eq!(open.render(40).len(), 30);
        let (mut regular, _) = view(20, 5, false);
        assert_eq!(plain(regular.render(40))[4], "lines 1-4 of 20");
    }
}
