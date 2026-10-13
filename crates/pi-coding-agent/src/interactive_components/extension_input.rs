//! [`ExtensionInput`]: a titled single-line text prompt for extensions
//! (upstream `extension-input.ts`).

use std::time::Duration;

use pi_tui::components::{Input, Text};
use pi_tui::{Component, Container, Tui, component, get_keybindings};

use super::common::{Shared, border, fg, shared, spacer, text};
use super::{CountdownTimer, key_hint};
use crate::theme::ThemeColor;

/// Optional behavior of an [`ExtensionInput`].
#[derive(Default)]
pub struct ExtensionInputOptions {
    pub tui: Option<Tui>,
    pub timeout: Option<Duration>,
    pub initial_value: Option<String>,
    pub description: Option<String>,
}

pub struct ExtensionInput {
    container: Container,
    input: std::rc::Rc<std::cell::RefCell<Input>>,
    on_submit: Box<dyn FnMut(&str)>,
    on_cancel: Shared<dyn FnMut()>,
    countdown: Option<CountdownTimer>,
}

impl ExtensionInput {
    /// `placeholder` is accepted for API parity and not shown, like upstream.
    pub fn new(
        title: &str,
        _placeholder: Option<&str>,
        on_submit: impl FnMut(&str) + 'static,
        on_cancel: impl FnMut() + 'static,
        opts: ExtensionInputOptions,
    ) -> Self {
        let on_cancel: Shared<dyn FnMut()> = shared(Box::new(on_cancel));
        let mut container = Container::new();
        container.add_child(border());
        container.add_child(spacer(1));
        let title_text = component(Text::new(&fg(ThemeColor::Accent, title), 1, 0));
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
                        title_text.borrow_mut().set_text(&fg(ThemeColor::Accent, &title));
                    },
                    move || (cancel.borrow_mut())(),
                ))
            }
            _ => None,
        };

        let mut input = Input::default();
        if let Some(value) = opts.initial_value.as_deref().filter(|value| !value.is_empty()) {
            input.set_value(value);
        }
        let input = component(input);
        container.add_child(input.clone());
        container.add_child(spacer(1));
        let hint = format!("{}  {}", key_hint("tui.select.confirm", "submit"), key_hint("tui.select.cancel", "cancel"));
        container.add_child(text(&hint, 1, 0));
        container.add_child(spacer(1));
        container.add_child(border());

        Self { container, input, on_submit: Box::new(on_submit), on_cancel, countdown }
    }

    pub fn value(&self) -> String {
        self.input.borrow().get_value().to_string()
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

impl Component for ExtensionInput {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.container.render(width)
    }

    fn handle_input(&mut self, data: &str) {
        let kb = get_keybindings();
        if kb.matches(data, "tui.select.confirm") || data == "\n" {
            let value = self.value();
            (self.on_submit)(&value);
        } else if kb.matches(data, "tui.select.cancel") {
            (self.on_cancel.borrow_mut())();
        } else {
            self.input.borrow_mut().handle_input(data);
        }
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn is_focusable(&self) -> bool {
        true
    }

    fn set_focused(&mut self, focused: bool) {
        self.input.borrow_mut().focused = focused;
    }

    fn invalidate(&mut self) {
        self.container.invalidate();
    }
}
