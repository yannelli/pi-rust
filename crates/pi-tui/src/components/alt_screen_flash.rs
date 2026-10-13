//! Transient messages drawn by the fullscreen renderer in the top-right corner.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use crate::component::Component;
use crate::ui_loop::{self, TimerId};
use crate::utils::truncate_to_width_with;

const DEFAULT_DURATION: Duration = Duration::from_millis(1000);

struct FlashEntry {
    id: u64,
    message: String,
    timer: TimerId,
}

#[derive(Default)]
struct FlashState {
    entries: Vec<FlashEntry>,
    next_id: u64,
}

/// Stack of flash messages that expire after their duration (upstream `AltScreenFlashContainer`).
pub struct AltScreenFlashContainer {
    state: Rc<RefCell<FlashState>>,
    request_render: Rc<dyn Fn()>,
}

impl AltScreenFlashContainer {
    pub fn new(request_render: Rc<dyn Fn()>) -> Self {
        Self { state: Rc::new(RefCell::new(FlashState::default())), request_render }
    }

    /// Shows `message` for `duration` (default one second).
    pub fn flash(&self, message: &str, duration: Option<Duration>) {
        let id = {
            let mut state = self.state.borrow_mut();
            state.next_id += 1;
            state.next_id
        };
        let weak = Rc::downgrade(&self.state);
        let request_render = self.request_render.clone();
        let timer = ui_loop::set_timeout(duration.unwrap_or(DEFAULT_DURATION), move || {
            let Some(state) = weak.upgrade() else { return };
            let removed = {
                let mut state = state.borrow_mut();
                let index = state.entries.iter().position(|e| e.id == id);
                index.map(|index| state.entries.remove(index)).is_some()
            };
            if removed {
                request_render();
            }
        });
        self.state.borrow_mut().entries.push(FlashEntry { id, message: message.to_string(), timer });
        (self.request_render)();
    }

    /// Drops every message and cancels the timers.
    pub fn dispose(&self) {
        let entries = std::mem::take(&mut self.state.borrow_mut().entries);
        for entry in entries {
            ui_loop::clear_timer(entry.timer);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.state.borrow().entries.is_empty()
    }

    /// Renders one reverse-video line per message.
    pub fn render_lines(&self, width: usize) -> Vec<String> {
        self.state
            .borrow()
            .entries
            .iter()
            .map(|entry| {
                let message = truncate_to_width_with(&format!(" {} ", entry.message), width, "", false);
                format!("\x1b[7m{message}\x1b[27m")
            })
            .collect()
    }
}

impl Component for AltScreenFlashContainer {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.render_lines(width)
    }
}
