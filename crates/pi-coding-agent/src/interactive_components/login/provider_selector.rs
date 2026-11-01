//! [`OAuthSelector`]: the `/login` and `/logout` provider list (upstream `oauth-selector.ts`).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use pi_ai::auth::{AuthCheck, AuthType};
use pi_tui::components::Input;
use pi_tui::{Component, Container, component, get_keybindings, get_select_page_index};

use crate::agent_session::login::LoginProvider;
use crate::interactive_components::common::{bold, border, fg, fuzzy_filter_by, spacer, truncated, visible_window};
use crate::theme::ThemeColor;

/// One login option of the selector (upstream `AuthSelectorProvider`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthSelectorProvider {
    pub id: String,
    pub name: String,
    pub auth_type: AuthType,
    /// Name of the auth method, such as "Anthropic API key"; part of the search text.
    pub method_name: Option<String>,
    pub status: Option<AuthCheck>,
    /// `Some(false)` labels OAuth as an account; otherwise it is a subscription.
    pub subscription: Option<bool>,
    /// An API key method configured outside pi, with no login to run.
    pub ambient: bool,
}

impl From<&LoginProvider> for AuthSelectorProvider {
    fn from(provider: &LoginProvider) -> Self {
        Self {
            id: provider.id.clone(),
            name: provider.name.clone(),
            auth_type: provider.auth_type,
            method_name: provider.login_label.clone(),
            status: provider.status.clone(),
            subscription: Some(provider.subscription),
            ambient: provider.ambient,
        }
    }
}

/// Whether the login selector lists or logs out providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthSelectorMode {
    Login,
    Logout,
}

fn auth_type_id(auth_type: AuthType) -> &'static str {
    match auth_type {
        AuthType::OAuth => "oauth",
        AuthType::ApiKey => "api_key",
    }
}

/// "API key", "account", or "subscription".
pub fn format_auth_selector_provider_type(auth_type: AuthType, subscription: Option<bool>) -> &'static str {
    match auth_type {
        AuthType::ApiKey => "API key",
        AuthType::OAuth if subscription == Some(false) => "account",
        AuthType::OAuth => "subscription",
    }
}

fn is_env_var_list(source: &str) -> bool {
    !source.is_empty()
        && source.split(", ").all(|name| {
            let mut chars = name.chars();
            chars.next().is_some_and(|first| first.is_ascii_uppercase())
                && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        })
}

/// Themed suffix describing whether and how an option is configured, such as " ✓ configured".
pub fn format_auth_selector_provider_status(provider: &AuthSelectorProvider) -> String {
    let Some(status) = &provider.status else { return fg(ThemeColor::Muted, " • not configured") };
    if status.auth_type != provider.auth_type {
        let label =
            format!("{} configured", format_auth_selector_provider_type(status.auth_type, provider.subscription));
        return fg(ThemeColor::Muted, " • ") + &fg(ThemeColor::Warning, &label);
    }
    let source = status.source.as_str();
    if source.is_empty() || source == "OAuth" || source == "stored credential" {
        return fg(ThemeColor::Success, " ✓ configured");
    }
    let source = if is_env_var_list(source) { format!("env: {source}") } else { source.to_string() };
    fg(ThemeColor::Success, &format!(" ✓ {source}"))
}

const MAX_VISIBLE: usize = 8;

/// Called with the chosen provider id and auth type.
pub type ProviderSelectFn = Box<dyn FnMut(&str, AuthType)>;

pub struct OAuthSelector {
    container: Container,
    search_input: Rc<RefCell<Input>>,
    submitted: Rc<Cell<bool>>,
    list: Rc<RefCell<Container>>,
    all_providers: Vec<AuthSelectorProvider>,
    filtered: Vec<AuthSelectorProvider>,
    selected_index: usize,
    mode: AuthSelectorMode,
    on_select: ProviderSelectFn,
    on_cancel: Box<dyn FnMut()>,
    show_auth_type_labels: bool,
}

impl OAuthSelector {
    pub fn new(
        mode: AuthSelectorMode,
        providers: Vec<AuthSelectorProvider>,
        on_select: impl FnMut(&str, AuthType) + 'static,
        on_cancel: impl FnMut() + 'static,
        initial_search_input: Option<&str>,
    ) -> Self {
        let show_auth_type_labels = providers
            .first()
            .is_some_and(|first| providers.iter().any(|provider| provider.auth_type != first.auth_type));
        let mut container = Container::new();
        container.add_child(border());
        container.add_child(spacer(1));
        let title = match mode {
            AuthSelectorMode::Login => "Select provider to configure:",
            AuthSelectorMode::Logout => "Select provider to logout:",
        };
        container.add_child(truncated(&fg(ThemeColor::Accent, &bold(title)), 1, 0));
        container.add_child(spacer(1));
        let submitted = Rc::new(Cell::new(false));
        let mut input = Input::default();
        if let Some(initial) = initial_search_input.filter(|initial| !initial.is_empty()) {
            input.set_value(initial);
        }
        let flag = submitted.clone();
        input.set_on_submit(move |_| flag.set(true));
        let search_input = component(input);
        container.add_child(search_input.clone());
        container.add_child(spacer(1));
        let list = component(Container::new());
        container.add_child(list.clone());
        container.add_child(spacer(1));
        container.add_child(border());

        let mut selector = Self {
            container,
            search_input,
            submitted,
            list,
            filtered: providers.clone(),
            all_providers: providers,
            selected_index: 0,
            mode,
            on_select: Box::new(on_select),
            on_cancel: Box::new(on_cancel),
            show_auth_type_labels,
        };
        selector.filter_providers(initial_search_input.unwrap_or(""));
        selector
    }

    fn filter_providers(&mut self, query: &str) {
        self.filtered = if query.is_empty() {
            self.all_providers.clone()
        } else {
            fuzzy_filter_by(&self.all_providers, query, |provider| {
                let method = provider.method_name.as_deref().unwrap_or("");
                format!("{} {} {} {method}", provider.name, provider.id, auth_type_id(provider.auth_type))
            })
        };
        self.selected_index = self.selected_index.min(self.filtered.len().saturating_sub(1));
        self.update_list();
    }

    fn update_list(&mut self) {
        let mut list = self.list.borrow_mut();
        list.clear();
        let (start, end) = visible_window(self.selected_index, self.filtered.len(), MAX_VISIBLE);
        for (index, provider) in self.filtered.iter().enumerate().take(end).skip(start) {
            let status = format_auth_selector_provider_status(provider);
            let auth_label = if self.show_auth_type_labels {
                let label = format_auth_selector_provider_type(provider.auth_type, provider.subscription);
                fg(ThemeColor::Muted, &format!(" [{label}]"))
            } else {
                String::new()
            };
            let line = if index == self.selected_index {
                format!(
                    "{}{}{auth_label}{status}",
                    fg(ThemeColor::Accent, "→ "),
                    fg(ThemeColor::Accent, &provider.name)
                )
            } else {
                format!("  {}{auth_label}{status}", fg(ThemeColor::Text, &provider.name))
            };
            list.add_child(truncated(&line, 1, 0));
        }
        if start > 0 || end < self.filtered.len() {
            let info = format!("  ({}/{})", self.selected_index + 1, self.filtered.len());
            list.add_child(truncated(&fg(ThemeColor::Muted, &info), 1, 0));
        }
        if self.filtered.is_empty() {
            let message = match (self.all_providers.is_empty(), self.mode) {
                (true, AuthSelectorMode::Login) => "No providers available",
                (true, AuthSelectorMode::Logout) => "No providers logged in. Use /login first.",
                (false, _) => "No matching providers",
            };
            list.add_child(truncated(&fg(ThemeColor::Muted, &format!("  {message}")), 1, 0));
        }
    }

    fn select_current(&mut self) {
        if let Some(provider) = self.filtered.get(self.selected_index) {
            let (id, auth_type) = (provider.id.clone(), provider.auth_type);
            (self.on_select)(&id, auth_type);
        }
    }
}

impl Component for OAuthSelector {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.container.render(width)
    }

    fn handle_input(&mut self, data: &str) {
        let kb = get_keybindings();
        let page_index = get_select_page_index(&kb, data, self.selected_index, self.filtered.len(), MAX_VISIBLE);
        if kb.matches(data, "tui.select.up") {
            if self.filtered.is_empty() {
                return;
            }
            self.selected_index = self.selected_index.saturating_sub(1);
            self.update_list();
        } else if kb.matches(data, "tui.select.down") {
            if self.filtered.is_empty() {
                return;
            }
            self.selected_index = (self.selected_index + 1).min(self.filtered.len() - 1);
            self.update_list();
        } else if let Some(page_index) = page_index {
            if self.filtered.is_empty() {
                return;
            }
            self.selected_index = page_index;
            self.update_list();
        } else if kb.matches(data, "tui.select.confirm") {
            self.select_current();
        } else if kb.matches(data, "tui.select.cancel") {
            (self.on_cancel)();
        } else {
            self.search_input.borrow_mut().handle_input(data);
            if self.submitted.replace(false) {
                self.select_current();
            }
            let query = self.search_input.borrow().get_value().to_string();
            self.filter_providers(&query);
        }
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn captures_viewport_input(&self) -> bool {
        true
    }

    fn is_focusable(&self) -> bool {
        true
    }

    fn set_focused(&mut self, focused: bool) {
        self.search_input.borrow_mut().focused = focused;
    }

    fn invalidate(&mut self) {
        self.container.invalidate();
    }
}
