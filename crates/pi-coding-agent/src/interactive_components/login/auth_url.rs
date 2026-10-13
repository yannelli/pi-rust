//! [`AuthUrl`]: a sign-in URL with click and copy hints (upstream `auth-url.ts`).

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use pi_tui::components::Text;
use pi_tui::terminal_image::hyperlink;
use pi_tui::{Component, Container, Tui, component};

use crate::interactive_components::common::fg;
use crate::interactive_components::key_hint;
use crate::theme::ThemeColor;

/// Copies text to the clipboard; resolves to an error message on failure.
pub type ClipboardCopyFn = Rc<dyn Fn(String) -> Pin<Box<dyn Future<Output = Result<(), String>>>>>;

/// Opens a URL in the browser.
pub type BrowserOpenFn = Rc<dyn Fn(&str)>;

/// The system clipboard, written on a blocking thread.
pub fn system_clipboard() -> ClipboardCopyFn {
    Rc::new(|text: String| {
        Box::pin(async move {
            match tokio::task::spawn_blocking(move || crate::clipboard::copy_to_clipboard(&text)).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => Err(error.to_string()),
                Err(error) => Err(error.to_string()),
            }
        })
    })
}

/// The system browser.
pub fn system_browser() -> BrowserOpenFn {
    Rc::new(pi_ai::auth::oauth::open_browser)
}

pub(crate) fn click_hint() -> &'static str {
    if cfg!(target_os = "macos") { "Cmd+click to open" } else { "Ctrl+click to open" }
}

/// A clickable URL plus a hint line. Hosts call [`AuthUrl::copy`] on `app.message.copy`, since a
/// long URL wraps and often cannot be selected or clicked as a whole (SSH, tmux).
pub struct AuthUrl {
    container: Container,
    url: String,
    tui: Tui,
    hint: Rc<RefCell<Text>>,
    copier: ClipboardCopyFn,
}

impl AuthUrl {
    pub fn new(tui: &Tui, url: &str) -> Self {
        Self::with_clipboard(tui, url, system_clipboard())
    }

    pub fn with_clipboard(tui: &Tui, url: &str, copier: ClipboardCopyFn) -> Self {
        let mut container = Container::new();
        container.add_child(component(Text::new(&fg(ThemeColor::Accent, &hyperlink(url, url)), 1, 0)));
        let hint = component(Text::new("", 1, 0));
        container.add_child(hint.clone());
        let auth_url = Self { container, url: url.to_string(), tui: tui.clone(), hint, copier };
        set_hint(&auth_url.hint, &auth_url.tui, &auth_url.url, &key_hint("app.message.copy", "to copy"));
        auth_url
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Copies the URL and reports the outcome in the hint line. Needs a `LocalSet`.
    pub fn copy(&self) {
        let future = (self.copier)(self.url.clone());
        let (hint, tui, url) = (self.hint.clone(), self.tui.clone(), self.url.clone());
        tokio::task::spawn_local(async move {
            let suffix = match future.await {
                Ok(()) => fg(ThemeColor::Success, "Copied URL to clipboard"),
                Err(message) => fg(ThemeColor::Error, &message),
            };
            set_hint(&hint, &tui, &url, &suffix);
        });
    }
}

fn set_hint(hint: &Rc<RefCell<Text>>, tui: &Tui, url: &str, suffix: &str) {
    let text = format!("{} {} {suffix}", fg(ThemeColor::Dim, &hyperlink(click_hint(), url)), fg(ThemeColor::Dim, "•"));
    hint.borrow_mut().set_text(&text);
    tui.request_render(false);
}

impl Component for AuthUrl {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.container.render(width)
    }

    fn invalidate(&mut self) {
        self.container.invalidate();
    }
}
