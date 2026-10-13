//! [`LoginDialog`]: the login flow UI that replaces the editor during `/login`
//! (upstream `login-dialog.ts`).

use std::cell::RefCell;
use std::future::Future;
use std::rc::Rc;

use pi_ai::auth::{AuthError, AuthInfoLink, AuthResult};
use pi_tui::components::Input;
use pi_tui::{Component, ComponentRef, Container, Tui, component, get_keybindings, same_component};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::auth_url::{AuthUrl, BrowserOpenFn, ClipboardCopyFn, click_hint, system_browser, system_clipboard};
use crate::interactive_components::common::{bold, border, fg, spacer, text};
use crate::interactive_components::extension_selector::{ExtensionSelector, ExtensionSelectorOptions};
use crate::interactive_components::key_hint;
use crate::theme::ThemeColor;

type Reply = oneshot::Sender<AuthResult<String>>;

/// Called with whether the login succeeded and a message.
pub type LoginCompleteFn = Box<dyn FnMut(bool, Option<&str>)>;

/// A select prompt shown in place of the dialog until an option is chosen.
struct ActiveSelect {
    selector: ExtensionSelector,
    choice: Rc<RefCell<Option<Option<String>>>>,
    reply: Reply,
}

/// Shows sign-in URLs, device codes, prompts, and progress for one login. Cancelling (the
/// cancel key) fires [`LoginDialog::cancel_token`] and calls `on_complete(false, "Login cancelled")`.
pub struct LoginDialog {
    container: Container,
    content: Rc<RefCell<Container>>,
    input: Rc<RefCell<Input>>,
    submitted: Rc<RefCell<Option<String>>>,
    tui: Tui,
    provider_id: String,
    auth_url: Option<Rc<RefCell<AuthUrl>>>,
    cancel: CancellationToken,
    pending: Option<Reply>,
    select: Option<ActiveSelect>,
    on_complete: LoginCompleteFn,
    open_browser: BrowserOpenFn,
    clipboard: ClipboardCopyFn,
}

impl LoginDialog {
    /// The title is `title_override`, else `Login to <provider name or id>`.
    pub fn new(
        tui: &Tui,
        provider_id: &str,
        on_complete: impl FnMut(bool, Option<&str>) + 'static,
        provider_name_override: Option<&str>,
        title_override: Option<&str>,
    ) -> Self {
        let provider_name = provider_name_override.filter(|name| !name.is_empty()).unwrap_or(provider_id);
        let title = title_override.map_or_else(|| format!("Login to {provider_name}"), str::to_string);
        let mut container = Container::new();
        container.add_child(border());
        container.add_child(text(&fg(ThemeColor::Accent, &bold(&title)), 1, 0));
        let content = component(Container::new());
        container.add_child(content.clone());
        container.add_child(border());

        let submitted: Rc<RefCell<Option<String>>> = Rc::default();
        let mut input = Input::default();
        let slot = submitted.clone();
        input.set_on_submit(move |value| *slot.borrow_mut() = Some(value.to_string()));

        Self {
            container,
            content,
            input: component(input),
            submitted,
            tui: tui.clone(),
            provider_id: provider_id.to_string(),
            auth_url: None,
            cancel: CancellationToken::new(),
            pending: None,
            select: None,
            on_complete: Box::new(on_complete),
            open_browser: system_browser(),
            clipboard: system_clipboard(),
        }
    }

    /// Replaces the browser opener (tests, or hosts without a browser).
    pub fn set_browser_opener(&mut self, open_browser: BrowserOpenFn) {
        self.open_browser = open_browser;
    }

    /// Replaces the clipboard used by the copy key.
    pub fn set_clipboard(&mut self, clipboard: ClipboardCopyFn) {
        self.clipboard = clipboard;
    }

    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    /// Fired when the user cancels; pass it to the login flow.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    fn input_ref(&self) -> ComponentRef {
        self.input.clone()
    }

    fn add(&self, child: ComponentRef) {
        self.content.borrow_mut().add_child(child);
    }

    fn cancel(&mut self) {
        self.cancel.cancel();
        if let Some(reply) = self.pending.take() {
            let _ = reply.send(Err(AuthError::Cancelled));
        }
        (self.on_complete)(false, Some("Login cancelled"));
    }

    fn submit(&mut self, value: String) {
        let Some(reply) = self.pending.take() else { return };
        let input = self.input_ref();
        let mut content = self.content.borrow_mut();
        for child in content.children.iter_mut() {
            if same_component(child, &input) {
                *child = text(&format!("> {value}"), 0, 0);
            }
        }
        drop(content);
        let _ = reply.send(Ok(value));
    }

    fn wait_for_input(&mut self) -> impl Future<Output = AuthResult<String>> + use<> {
        let (reply, receiver) = oneshot::channel();
        if let Some(previous) = self.pending.replace(reply) {
            let _ = previous.send(Err(AuthError::Cancelled));
        }
        async move { receiver.await.unwrap_or(Err(AuthError::Cancelled)) }
    }

    /// Shows the sign-in URL and instructions, and opens the browser.
    pub fn show_auth(&mut self, url: &str, instructions: Option<&str>) {
        self.content.borrow_mut().clear();
        self.add(spacer(1));
        let auth_url = component(AuthUrl::with_clipboard(&self.tui, url, self.clipboard.clone()));
        self.add(auth_url.clone());
        self.auth_url = Some(auth_url);
        if let Some(instructions) = instructions.filter(|instructions| !instructions.is_empty()) {
            self.add(spacer(1));
            self.add(text(&fg(ThemeColor::Warning, instructions), 1, 0));
        }
        (self.open_browser)(url);
        self.tui.request_render(false);
    }

    /// Shows the verification URL and the code to enter.
    pub fn show_device_code(&mut self, user_code: &str, verification_uri: &str) {
        self.auth_url = None;
        self.content.borrow_mut().clear();
        self.add(spacer(1));
        self.add(text(&fg(ThemeColor::Accent, &hyperlink_bel(verification_uri, verification_uri)), 1, 0));
        self.add(text(&fg(ThemeColor::Dim, &hyperlink_bel(click_hint(), verification_uri)), 1, 0));
        self.add(spacer(1));
        self.add(text(&fg(ThemeColor::Warning, &format!("Enter code: {user_code}")), 1, 0));
        self.tui.request_render(false);
    }

    /// Asks for a pasted code or redirect URL; resolves with the submitted text.
    pub fn show_manual_input(&mut self, prompt: &str) -> impl Future<Output = AuthResult<String>> + use<> {
        self.input.borrow_mut().set_value("");
        self.add(spacer(1));
        self.add(text(&fg(ThemeColor::Dim, prompt), 1, 0));
        self.add(self.input_ref());
        self.add(text(&format!("({})", key_hint("tui.select.cancel", "to cancel")), 1, 0));
        self.tui.request_render(false);
        self.wait_for_input()
    }

    /// Appends a prompt (keeping a shown URL) and resolves with the submitted text.
    pub fn show_prompt(
        &mut self,
        message: &str,
        placeholder: Option<&str>,
    ) -> impl Future<Output = AuthResult<String>> + use<> {
        self.add(spacer(1));
        self.add(text(&fg(ThemeColor::Text, message), 1, 0));
        if let Some(placeholder) = placeholder.filter(|placeholder| !placeholder.is_empty()) {
            self.add(text(&fg(ThemeColor::Dim, &format!("e.g., {placeholder}")), 1, 0));
        }
        self.add(self.input_ref());
        let hint = format!(
            "({} {})",
            key_hint("tui.select.cancel", "to cancel,"),
            key_hint("tui.select.confirm", "to submit")
        );
        self.add(text(&hint, 1, 0));
        self.input.borrow_mut().set_value("");
        self.tui.request_render(false);
        self.wait_for_input()
    }

    /// Replaces the content with informational lines.
    pub fn show_details(&mut self, lines: &[String]) {
        self.auth_url = None;
        self.content.borrow_mut().clear();
        self.add(spacer(1));
        for line in lines {
            self.add(text(line, 1, 0));
        }
        self.tui.request_render(false);
    }

    /// Appends provider information and links, optionally with a close hint.
    pub fn show_info(&mut self, message: &str, links: &[AuthInfoLink], show_close_hint: bool) {
        self.add(spacer(1));
        self.add(text(&fg(ThemeColor::Text, message), 1, 0));
        for link in links {
            let label =
                link.label.as_deref().map_or_else(|| link.url.clone(), |label| format!("{label}: {}", link.url));
            self.add(text(&fg(ThemeColor::Accent, &hyperlink_bel(&label, &link.url)), 1, 0));
        }
        if show_close_hint {
            self.add(spacer(1));
            self.add(text(&format!("({})", key_hint("tui.select.cancel", "to close")), 1, 0));
        }
        self.tui.request_render(false);
    }

    /// Appends a waiting message for polling flows.
    pub fn show_waiting(&mut self, message: &str) {
        self.add(spacer(1));
        self.add(text(&fg(ThemeColor::Dim, message), 1, 0));
        self.add(text(&format!("({})", key_hint("tui.select.cancel", "to cancel")), 1, 0));
        self.tui.request_render(false);
    }

    /// Appends a progress line.
    pub fn show_progress(&mut self, message: &str) {
        self.add(text(&fg(ThemeColor::Dim, message), 1, 0));
        self.tui.request_render(false);
    }

    /// Shows a list of options in place of the dialog; resolves with the chosen label, or
    /// `Cancelled` when the list is cancelled. The dialog returns afterwards.
    pub fn show_select(
        &mut self,
        message: &str,
        options: Vec<String>,
        description: Option<&str>,
    ) -> impl Future<Output = AuthResult<String>> + use<> {
        let choice: Rc<RefCell<Option<Option<String>>>> = Rc::default();
        let (on_select, on_cancel) = (choice.clone(), choice.clone());
        let opts = ExtensionSelectorOptions { description: description.map(str::to_string), ..Default::default() };
        let selector = ExtensionSelector::new(
            message,
            options,
            move |option: &str| *on_select.borrow_mut() = Some(Some(option.to_string())),
            move || *on_cancel.borrow_mut() = Some(None),
            opts,
        );
        let (reply, receiver) = oneshot::channel();
        if let Some(previous) = self.select.replace(ActiveSelect { selector, choice, reply }) {
            let _ = previous.reply.send(Err(AuthError::Cancelled));
        }
        self.tui.request_render(false);
        async move { receiver.await.unwrap_or(Err(AuthError::Cancelled)) }
    }

    fn handle_select_input(&mut self, data: &str) {
        let Some(active) = self.select.as_mut() else { return };
        active.selector.handle_input(data);
        let Some(choice) = active.choice.borrow_mut().take() else { return };
        let active = self.select.take().expect("active select");
        let _ = active.reply.send(choice.ok_or(AuthError::Cancelled));
        self.tui.request_render(false);
    }
}

/// An OSC 8 link terminated with BEL, as upstream writes it by hand.
fn hyperlink_bel(text: &str, url: &str) -> String {
    format!("\x1b]8;;{url}\x07{text}\x1b]8;;\x07")
}

impl Component for LoginDialog {
    fn render(&mut self, width: usize) -> Vec<String> {
        match self.select.as_mut() {
            Some(active) => active.selector.render(width),
            None => self.container.render(width),
        }
    }

    fn handle_input(&mut self, data: &str) {
        if self.select.is_some() {
            self.handle_select_input(data);
            return;
        }
        let kb = get_keybindings();
        if kb.matches(data, "tui.select.cancel") {
            self.cancel();
            return;
        }
        if let Some(auth_url) = &self.auth_url
            && kb.matches(data, "app.message.copy")
        {
            auth_url.borrow().copy();
            return;
        }
        self.input.borrow_mut().handle_input(data);
        let submitted = self.submitted.borrow_mut().take();
        if let Some(value) = submitted {
            self.submit(value);
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
