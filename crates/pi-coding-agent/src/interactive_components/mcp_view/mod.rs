//! The `/mcp` manager view (upstream `extensions/mcp/ui.ts`): menus that rebuild while servers
//! connect, a status screen that can cancel, and the sign-in screen that takes a pasted redirect
//! URL. [`run_mcp_manager`] drives it over [`McpServers`] (upstream `manage` in `index.ts`).

mod flow;
mod servers;

use std::cell::RefCell;
use std::rc::Rc;

use pi_tui::components::{Input, InputOptions, SelectItem, SelectList, SelectListLayoutOptions};
use pi_tui::{Component, ComponentRef, Container, Tui, component, get_keybindings};

pub use flow::{run_mcp_manager, show_menu, sign_in_with_view};
pub use servers::{EXPOSURE_DESCRIPTIONS, McpServers, exposure_menu, server_menu, servers_menu, tools_menu};

use super::common::{clamp_lines, fg, frame, spacer, text};
use super::key_hint;
use super::login::{AuthUrl, ClipboardCopyFn};
use crate::theme::{ThemeColor, get_select_list_theme};

const MAX_VISIBLE_ITEMS: usize = 12;

/// One menu screen (upstream `McpMenu`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct McpMenu {
    pub title: String,
    /// Shown below the title.
    pub details: Option<String>,
    /// Shown below the details in the error color.
    pub error: Option<String>,
    pub items: Vec<SelectItem>,
    /// Shown when there are no items.
    pub empty: Option<String>,
    /// Value of the item selected when the menu opens.
    pub selected: Option<String>,
    /// What the confirm key does, for the key hint.
    pub confirm_label: String,
    /// What the cancel key does, for the key hint.
    pub cancel_label: String,
}

/// Builds a menu from the current server state; live menus call it again on every change.
pub type McpMenuBuilder = Rc<dyn Fn() -> McpMenu>;

type Finish = Rc<RefCell<Option<Box<dyn FnOnce(Option<String>)>>>>;
/// Where the view sends input: a shown menu list, which also captures viewport keys, or a key handler.
enum InputHandler {
    List(ComponentRef),
    Keys(Box<dyn FnMut(&str)>),
}

fn finish(slot: &Finish, value: Option<String>) {
    let callback = slot.borrow_mut().take();
    if let Some(callback) = callback {
        callback(value);
    }
}

struct ActiveMenu {
    build: McpMenuBuilder,
    selected: Rc<RefCell<Option<String>>>,
    finish: Finish,
    live: bool,
}

/// Upstream `McpManagerView`.
pub struct McpManagerView {
    tui: Tui,
    content: Container,
    input_handler: Option<InputHandler>,
    input_target: Option<ComponentRef>,
    focused: bool,
    menu: Option<ActiveMenu>,
    clipboard: Option<ClipboardCopyFn>,
}

impl McpManagerView {
    /// Starts on a loading frame.
    pub fn new(tui: &Tui) -> Self {
        let loading = text(&fg(ThemeColor::Muted, "Loading…"), 1, 1);
        Self {
            tui: tui.clone(),
            content: frame("MCP servers", vec![loading], None),
            input_handler: None,
            input_target: None,
            focused: false,
            menu: None,
            clipboard: None,
        }
    }

    /// Copies sign-in URLs with `copier` instead of the system clipboard.
    pub fn set_clipboard(&mut self, copier: ClipboardCopyFn) {
        self.clipboard = Some(copier);
    }

    fn set_content(&mut self, content: Container, handler: Option<InputHandler>, target: Option<ComponentRef>) {
        if let Some(target) = &self.input_target {
            target.borrow_mut().set_focused(false);
        }
        self.content = content;
        self.input_handler = handler;
        self.input_target = target;
        if let Some(target) = &self.input_target {
            target.borrow_mut().set_focused(self.focused);
        }
        self.tui.request_render(false);
    }

    /// Shows the menu `build` returns; `on_choice` gets the chosen value, or `None` when
    /// cancelled. A `live` menu rebuilds on [`Self::rebuild_menu`], keeping the selected item.
    pub fn menu(&mut self, build: McpMenuBuilder, live: bool, on_choice: impl FnOnce(Option<String>) + 'static) {
        let finish: Finish = Rc::new(RefCell::new(Some(Box::new(on_choice))));
        self.menu = Some(ActiveMenu { build, selected: Rc::default(), finish, live });
        self.render_menu();
    }

    /// Rebuilds a live menu that is still open (upstream `subscribe` listener).
    pub fn rebuild_menu(&mut self) {
        if self.menu.as_ref().is_some_and(|menu| menu.live) {
            self.render_menu();
        }
    }

    fn render_menu(&mut self) {
        let Some(active) = &self.menu else { return };
        if active.finish.borrow().is_none() {
            return;
        }
        let menu = (active.build)();
        let (selected, slot) = (active.selected.clone(), active.finish.clone());
        let wanted = selected.borrow().clone().or(menu.selected.clone());
        let mut body = Vec::new();
        if let Some(details) = menu.details.as_deref().filter(|details| !details.is_empty()) {
            body.push(text(&fg(ThemeColor::Muted, details), 1, 0));
        }
        if let Some(error) = menu.error.as_deref().filter(|error| !error.is_empty()) {
            body.push(text(&fg(ThemeColor::Error, error), 1, 0));
        }
        body.push(spacer(1));
        let cancel_hint = key_hint("tui.select.cancel", &menu.cancel_label);
        if menu.items.is_empty() {
            let empty = menu.empty.as_deref().unwrap_or("Nothing to show.");
            body.push(text(&fg(ThemeColor::Muted, empty), 1, 0));
            let handler = move |data: &str| {
                if get_keybindings().matches(data, "tui.select.cancel") {
                    finish(&slot, None);
                }
            };
            let handler = InputHandler::Keys(Box::new(handler));
            self.set_content(frame(&menu.title, body, Some(&cancel_hint)), Some(handler), None);
            return;
        }
        let visible = menu.items.len().min(MAX_VISIBLE_ITEMS);
        let index = menu.items.iter().position(|item| Some(&item.value) == wanted.as_ref());
        let mut list =
            SelectList::new(menu.items.clone(), visible, get_select_list_theme(), SelectListLayoutOptions::default());
        if let Some(index) = index {
            list.set_selected_index(index);
        }
        *selected.borrow_mut() = list.get_selected_item().map(|item| item.value.clone());
        list.on_selection_change = Some(Box::new(move |item: &SelectItem| {
            *selected.borrow_mut() = Some(item.value.clone());
        }));
        let on_select = slot.clone();
        list.on_select = Some(Box::new(move |item: &SelectItem| finish(&on_select, Some(item.value.clone()))));
        list.on_cancel = Some(Box::new(move || finish(&slot, None)));
        let list = component(list);
        body.push(list.clone());
        let footer = format!("{} • {cancel_hint}", key_hint("tui.select.confirm", &menu.confirm_label));
        self.set_content(frame(&menu.title, body, Some(&footer)), Some(InputHandler::List(list)), None);
    }

    /// A message while an operation runs; with `on_cancel`, the cancel key calls it.
    pub fn status(&mut self, title: &str, message: &str, on_cancel: Option<Box<dyn FnMut()>>) {
        self.menu = None;
        let body = vec![spacer(1), text(&fg(ThemeColor::Muted, message), 1, 0)];
        let Some(mut on_cancel) = on_cancel else {
            self.set_content(frame(title, body, None), None, None);
            return;
        };
        let handler = move |data: &str| {
            if get_keybindings().matches(data, "tui.select.cancel") {
                on_cancel();
            }
        };
        let footer = key_hint("tui.select.cancel", "cancel");
        self.set_content(frame(title, body, Some(&footer)), Some(InputHandler::Keys(Box::new(handler))), None);
    }

    /// The authorization URL with a copy key and an input for the redirected URL; `on_done`
    /// gets the trimmed URL, or `None` when cancelled.
    pub fn redirect_url(
        &mut self,
        title: &str,
        authorization_url: &str,
        on_done: impl FnOnce(Option<String>) + 'static,
    ) {
        self.menu = None;
        let slot: Finish = Rc::new(RefCell::new(Some(Box::new(on_done))));
        let input = component(Input::new(InputOptions::default()));
        let link = match &self.clipboard {
            Some(copier) => AuthUrl::with_clipboard(&self.tui, authorization_url, copier.clone()),
            None => AuthUrl::new(&self.tui, authorization_url),
        };
        let link = component(link);
        let body: Vec<ComponentRef> = vec![
            spacer(1),
            text(&fg(ThemeColor::Muted, "Approve access in your browser. If it did not open, visit:"), 1, 0),
            link.clone(),
            spacer(1),
            text(
                &fg(ThemeColor::Muted, "If the browser runs on another machine, paste the URL it was redirected to:"),
                1,
                0,
            ),
            input.clone(),
        ];
        let footer =
            format!("{} • {}", key_hint("tui.select.confirm", "submit"), key_hint("tui.select.cancel", "cancel"));
        let field = input.clone();
        let handler = move |data: &str| {
            let keybindings = get_keybindings();
            if keybindings.matches(data, "tui.select.confirm") {
                let value = field.borrow().get_value().trim().to_string();
                if !value.is_empty() {
                    finish(&slot, Some(value));
                }
            } else if keybindings.matches(data, "tui.select.cancel") {
                finish(&slot, None);
            } else if keybindings.matches(data, "app.message.copy") {
                link.borrow().copy();
            } else {
                field.borrow_mut().handle_input(data);
            }
        };
        self.set_content(frame(title, body, Some(&footer)), Some(InputHandler::Keys(Box::new(handler))), Some(input));
    }
}

impl Component for McpManagerView {
    fn render(&mut self, width: usize) -> Vec<String> {
        clamp_lines(self.content.render(width), width)
    }

    fn handle_input(&mut self, data: &str) {
        match &mut self.input_handler {
            Some(InputHandler::List(list)) => list.borrow_mut().handle_input(data),
            Some(InputHandler::Keys(handler)) => handler(data),
            None => {}
        }
        self.tui.request_render(false);
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn captures_viewport_input(&self) -> bool {
        matches!(&self.input_handler, Some(InputHandler::List(list)) if list.borrow().captures_viewport_input())
    }

    fn is_focusable(&self) -> bool {
        true
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        if let Some(target) = &self.input_target {
            target.borrow_mut().set_focused(focused);
        }
    }

    fn invalidate(&mut self) {
        self.content.invalidate();
    }
}
