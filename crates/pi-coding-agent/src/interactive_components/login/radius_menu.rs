//! The `/login` menu whose "Sign in with Radius" option shimmers in the Radius logo colors while
//! selected (upstream `radius-login-selector.ts`).

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use pi_tui::colors::{Color, ColorMixSpace, foreground_ansi, mix_colors, parse_color};
use pi_tui::components::Text;
use pi_tui::ui_loop::{self, TimerId};
use pi_tui::{Component, Tui};

use crate::interactive_components::common::fg;
use crate::interactive_components::extension_selector::{ExtensionSelector, ExtensionSelectorOptions};
use crate::theme::{ThemeColor, theme};

const RADIUS_HEX: [&str; 4] = ["#4d9abf", "#83ccd2", "#f1be57", "#f09082"];
const CHARS_PER_COLOR: f64 = 4.0;
const CHARS_PER_SECOND: f64 = 10.0;
const ANIMATION_FRAME: Duration = Duration::from_millis(50);

fn radius_colors() -> Vec<Color> {
    RADIUS_HEX.iter().filter_map(|hex| parse_color(hex).ok()).collect()
}

/// Colors `text` with the logo colors flowing left to right at `elapsed`.
pub fn radius_shimmer(text: &str, elapsed: Duration) -> String {
    let colors = radius_colors();
    let mode = theme().get_color_mode();
    let cycle = colors.len() as f64 * CHARS_PER_COLOR;
    let offset = elapsed.as_secs_f64() * CHARS_PER_SECOND;
    let mut result = String::new();
    for (index, ch) in text.chars().enumerate() {
        let position = ((index as f64 - offset) % cycle + cycle) % cycle;
        let band = (position / CHARS_PER_COLOR).floor();
        let t = position / CHARS_PER_COLOR - band;
        let amount = t * t * (3.0 - 2.0 * t);
        let band = band as usize % colors.len();
        let (from, to) = (&colors[band], &colors[(band + 1) % colors.len()]);
        let mixed = mix_colors(from, to, amount.clamp(0.0, 1.0), ColorMixSpace::Srgb).unwrap_or(*from);
        result.push_str(&foreground_ansi(&mixed, mode));
        result.push(ch);
    }
    result.push_str("\x1b[39m");
    result
}

/// The Radius option: `label` is the whole option text, starting with the animated `text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RadiusOption {
    pub label: String,
    pub text: String,
}

/// Upstream `createLoginMenuSelector`: an [`ExtensionSelector`] that animates the selected
/// Radius option. When the row no longer matches (wrapping, restyling) it renders normally.
pub struct RadiusLoginMenu {
    pub(crate) selector: ExtensionSelector,
    radius_option: RadiusOption,
    started: Instant,
    timer: Rc<Cell<Option<TimerId>>>,
    animating: Rc<Cell<bool>>,
}

impl RadiusLoginMenu {
    pub fn new(
        tui: &Tui,
        title: &str,
        options: Vec<String>,
        radius_option: RadiusOption,
        mut on_select: impl FnMut(&str) + 'static,
        mut on_cancel: impl FnMut() + 'static,
    ) -> Self {
        let timer: Rc<Cell<Option<TimerId>>> = Rc::default();
        let animating = Rc::new(Cell::new(false));
        let stop = stopper(&timer, &animating);
        let stop_cancel = stop.clone();
        let selector = ExtensionSelector::new(
            title,
            options,
            move |option: &str| {
                stop();
                on_select(option);
            },
            move || {
                stop_cancel();
                on_cancel();
            },
            ExtensionSelectorOptions::default(),
        );
        let (tui, flag) = (tui.clone(), animating.clone());
        timer.set(Some(ui_loop::set_interval(ANIMATION_FRAME, move || {
            if flag.get() {
                tui.request_render(false);
            }
        })));
        Self { selector, radius_option, started: Instant::now(), timer, animating }
    }

    pub fn selector(&self) -> &ExtensionSelector {
        &self.selector
    }

    pub fn dispose(&mut self) {
        stopper(&self.timer, &self.animating)();
        self.selector.dispose();
    }

    /// Stops the animation; usable from inside a callback.
    pub fn disposer(&self) -> impl Fn() + 'static {
        let stop = stopper(&self.timer, &self.animating);
        let dispose_selector = self.selector.disposer();
        move || {
            stop();
            dispose_selector();
        }
    }
}

fn stopper(timer: &Rc<Cell<Option<TimerId>>>, animating: &Rc<Cell<bool>>) -> Rc<dyn Fn()> {
    let (timer, animating) = (timer.clone(), animating.clone());
    Rc::new(move || {
        if let Some(id) = timer.take() {
            ui_loop::clear_timer(id);
        }
        animating.set(false);
    })
}

impl Drop for RadiusLoginMenu {
    fn drop(&mut self) {
        stopper(&self.timer, &self.animating)();
    }
}

impl Component for RadiusLoginMenu {
    fn render(&mut self, width: usize) -> Vec<String> {
        let mut lines = self.selector.render(width);
        let RadiusOption { label, text } = &self.radius_option;
        let selected = fg(ThemeColor::Accent, "→ ") + &fg(ThemeColor::Accent, label);
        let selected_line = Text::new(&selected, 1, 0).render(width).into_iter().next();
        let index = selected_line.and_then(|line| lines.iter().position(|candidate| *candidate == line));
        self.animating.set(index.is_some() && self.timer.get().is_some());
        if let Some(index) = index {
            let shimmer = radius_shimmer(text, self.started.elapsed());
            let rest = label.get(text.len()..).unwrap_or("");
            let animated = fg(ThemeColor::Accent, "→ ") + &shimmer + rest;
            lines[index] = Text::new(&animated, 1, 0).render(width).into_iter().next().unwrap_or_default();
        }
        lines
    }

    fn handle_input(&mut self, data: &str) {
        self.selector.handle_input(data);
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn invalidate(&mut self) {
        self.selector.invalidate();
    }
}
