//! [`Loader`]: a message with an optional animated spinner.

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::time::Duration;

use crate::component::Component;
use crate::components::scroll_view::StyleFn;
use crate::components::text::Text;
use crate::tui::{Tui, WeakTui};
use crate::ui_loop::{self, TimerId};

const DEFAULT_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const DEFAULT_INTERVAL_MS: u64 = 80;

/// Indicator of a [`Loader`]. Passing one renders its frames verbatim (without the spinner color).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoaderIndicatorOptions {
    /// Animation frames. Use an empty list to hide the indicator.
    pub frames: Option<Vec<String>>,
    /// Frame interval in milliseconds for animated indicators.
    pub interval_ms: Option<u64>,
}

struct LoaderState {
    text: Text,
    frames: Vec<String>,
    interval_ms: u64,
    current_frame: usize,
    render_indicator_verbatim: bool,
    spinner_color_fn: StyleFn,
    message_color_fn: StyleFn,
    message: String,
    ui: Option<WeakTui>,
}

impl LoaderState {
    fn rendered_indicator(&self) -> String {
        let frame = self.frames.get(self.current_frame).map(String::as_str).unwrap_or("");
        if self.render_indicator_verbatim { frame.to_string() } else { (self.spinner_color_fn)(frame) }
    }

    fn update_display(&mut self) {
        let frame = self.rendered_indicator();
        let indicator = if frame.is_empty() { String::new() } else { format!("{frame} ") };
        let message = (self.message_color_fn)(&self.message);
        self.text.set_text(&format!("{indicator}{message}"));
        if let Some(tui) = self.ui.as_ref().and_then(WeakTui::upgrade) {
            tui.request_render(false);
        }
    }
}

/// A loader line preceded by an empty line (upstream `Loader`). The animation runs on the UI loop
/// timers ([`ui_loop::set_interval`]), so create it on the UI thread.
pub struct Loader {
    state: Rc<RefCell<LoaderState>>,
    interval: Option<TimerId>,
}

impl Loader {
    pub fn new(
        ui: Option<&Tui>,
        spinner_color_fn: StyleFn,
        message_color_fn: StyleFn,
        message: &str,
        indicator: Option<LoaderIndicatorOptions>,
    ) -> Self {
        let state = LoaderState {
            text: Text::new("", 1, 0),
            frames: Vec::new(),
            interval_ms: DEFAULT_INTERVAL_MS,
            current_frame: 0,
            render_indicator_verbatim: false,
            spinner_color_fn,
            message_color_fn,
            message: message.to_string(),
            ui: ui.map(Tui::downgrade),
        };
        let mut loader = Self { state: Rc::new(RefCell::new(state)), interval: None };
        loader.set_indicator(indicator);
        loader
    }

    /// Shows the current frame and (re)starts the animation.
    pub fn start(&mut self) {
        self.state.borrow_mut().update_display();
        self.restart_animation();
    }

    /// Stops the animation.
    pub fn stop(&mut self) {
        if let Some(id) = self.interval.take() {
            ui_loop::clear_timer(id);
        }
    }

    pub fn set_message(&mut self, message: &str) {
        let mut state = self.state.borrow_mut();
        state.message = message.to_string();
        state.update_display();
    }

    pub fn message(&self) -> String {
        self.state.borrow().message.clone()
    }

    /// The current indicator frame, styled like the rendered line (upstream `getRenderedIndicator`).
    pub fn rendered_indicator(&self) -> String {
        self.state.borrow().rendered_indicator()
    }

    /// Replaces the indicator. `None` restores the default spinner.
    pub fn set_indicator(&mut self, indicator: Option<LoaderIndicatorOptions>) {
        {
            let mut state = self.state.borrow_mut();
            state.render_indicator_verbatim = indicator.is_some();
            let indicator = indicator.unwrap_or_default();
            state.frames =
                indicator.frames.unwrap_or_else(|| DEFAULT_FRAMES.iter().map(|frame| frame.to_string()).collect());
            state.interval_ms = indicator.interval_ms.filter(|ms| *ms > 0).unwrap_or(DEFAULT_INTERVAL_MS);
            state.current_frame = 0;
        }
        self.start();
    }

    fn restart_animation(&mut self) {
        self.stop();
        let (frame_count, interval_ms) = {
            let state = self.state.borrow();
            (state.frames.len(), state.interval_ms)
        };
        if frame_count <= 1 {
            return;
        }
        let weak: Weak<RefCell<LoaderState>> = Rc::downgrade(&self.state);
        self.interval = Some(ui_loop::set_interval(Duration::from_millis(interval_ms), move || {
            if let Some(state) = weak.upgrade() {
                let mut state = state.borrow_mut();
                state.current_frame = (state.current_frame + 1) % state.frames.len().max(1);
                state.update_display();
            }
        }));
    }
}

impl Drop for Loader {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Component for Loader {
    fn render(&mut self, width: usize) -> Vec<String> {
        let mut lines = vec![String::new()];
        lines.extend(self.state.borrow_mut().text.render(width));
        lines
    }

    fn invalidate(&mut self) {
        let mut state = self.state.borrow_mut();
        state.text.invalidate();
        state.update_display();
    }
}
