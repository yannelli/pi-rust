//! A loader between two borders, optionally cancellable with Escape (upstream
//! `components/bordered-loader.ts`), used by `/share` and extension UI.

use std::rc::Rc;

use pi_tui::components::StyleFn;
use pi_tui::{CancellableLoader, Component, Container, Loader, Spacer, Text, Tui, component};
use tokio_util::sync::CancellationToken;

use crate::interactive_components::{DynamicBorder, key_hint};
use crate::theme::{ThemeColor, theme};

enum Inner {
    Cancellable(Rc<std::cell::RefCell<CancellableLoader>>),
    Plain(Rc<std::cell::RefCell<Loader>>),
}

pub struct BorderedLoader {
    root: Container,
    loader: Inner,
    token: CancellationToken,
}

fn style(color: ThemeColor) -> StyleFn {
    Rc::new(move |text: &str| theme().fg(color, text))
}

impl BorderedLoader {
    pub fn new(tui: &Tui, message: &str, cancellable: bool) -> Self {
        let mut root = Container::new();
        root.add_child(component(DynamicBorder::with_color(|text| theme().fg(ThemeColor::Border, text))));
        let (spinner, muted) = (style(ThemeColor::Accent), style(ThemeColor::Muted));
        let (loader, token) = if cancellable {
            let loader = component(CancellableLoader::new(Some(tui), spinner, muted, message, None));
            let token = loader.borrow().signal();
            root.add_child(loader.clone());
            (Inner::Cancellable(loader), token)
        } else {
            let loader = component(Loader::new(Some(tui), spinner, muted, message, None));
            root.add_child(loader.clone());
            (Inner::Plain(loader), CancellationToken::new())
        };
        if cancellable {
            root.add_child(component(Spacer::new(1)));
            root.add_child(component(Text::new(&key_hint("tui.select.cancel", "cancel"), 1, 0)));
        }
        root.add_child(component(Spacer::new(1)));
        root.add_child(component(DynamicBorder::with_color(|text| theme().fg(ThemeColor::Border, text))));
        Self { root, loader, token }
    }

    /// Cancelled when the user presses the cancel key.
    pub fn signal(&self) -> CancellationToken {
        self.token.clone()
    }

    pub fn set_on_abort(&self, on_abort: Option<Box<dyn FnMut()>>) {
        if let Inner::Cancellable(loader) = &self.loader {
            loader.borrow_mut().on_abort = on_abort;
        }
    }

    pub fn dispose(&self) {
        match &self.loader {
            Inner::Cancellable(loader) => loader.borrow_mut().dispose(),
            Inner::Plain(loader) => loader.borrow_mut().stop(),
        }
    }
}

impl Component for BorderedLoader {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.root.render(width)
    }

    fn handle_input(&mut self, data: &str) {
        if let Inner::Cancellable(loader) = &self.loader {
            loader.borrow_mut().handle_input(data);
        }
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn invalidate(&mut self) {
        self.root.invalidate();
    }
}
