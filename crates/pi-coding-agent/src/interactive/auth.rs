//! `/login` and `/logout`: the method menu, the provider selector, the login dialog, and the
//! model selection after a login (upstream `handleLoginCommand` and its helpers).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use pi_ai::Model;
use pi_ai::auth::{AuthError, AuthType, LoginOptions};
use pi_tui::fuzzy::fuzzy_filter;
use pi_tui::program_status::BlockedKind;
use pi_tui::{AutocompleteItem, Component, component};

use super::mode::InteractiveMode;
use super::selector::SelectorSpec;
use crate::agent_session::login::{NO_STORED_CREDENTIALS, logout_message};
use crate::interactive_components::login::{
    AuthSelectorMode, AuthSelectorProvider, LoginDialog, LoginMethodChoice, LoginMethodMenu, OAuthSelector,
    RadiusLoginMenu, api_key_login_details, format_auth_selector_provider_type, llama_cpp_post_login_guidance,
    login_action_label, no_login_providers_message, run_dialog_login,
};
use crate::interactive_components::{ExtensionSelector, ExtensionSelectorOptions};
use crate::llama::LLAMA_PROVIDER_ID;
use crate::model_resolver::default_model_for;

const RADIUS_PROVIDER_ID: &str = "radius";
const ANTHROPIC_SUBSCRIPTION_AUTH_WARNING: &str = "Anthropic subscription auth is active. Third-party harness usage draws from extra usage and is billed per token, not your Claude plan limits. Manage extra usage at https://claude.ai/settings/usage. Disable this warning in /settings.";

type OnBack = Option<Rc<dyn Fn()>>;

fn auth_type_order(auth_type: AuthType) -> u8 {
    if auth_type == AuthType::OAuth { 0 } else { 1 }
}

/// One completion per provider id with its methods (upstream `getLoginProviderCompletionOptions`).
fn login_completion_options(providers: &[AuthSelectorProvider]) -> Vec<(AuthSelectorProvider, Vec<AuthType>)> {
    let mut options: Vec<(AuthSelectorProvider, Vec<AuthType>)> = Vec::new();
    for provider in providers {
        match options.iter_mut().find(|(known, _)| known.id == provider.id) {
            Some((_, types)) => {
                if !types.contains(&provider.auth_type) {
                    types.push(provider.auth_type);
                    types.sort_by_key(|auth_type| auth_type_order(*auth_type));
                }
            }
            None => options.push((provider.clone(), vec![provider.auth_type])),
        }
    }
    options.sort_by_key(|(provider, _)| provider.name.to_lowercase());
    options
}

fn auth_type_key(auth_type: AuthType) -> &'static str {
    if auth_type == AuthType::OAuth { "oauth" } else { "api_key" }
}

impl InteractiveMode {
    /// Fills the `/login` completion cache in the background.
    pub(super) fn refresh_login_providers(&self) {
        let runtime = self.session().model_runtime().clone();
        self.spawn(move |mode| async move {
            let task = tokio::spawn(async move { runtime.login_providers(None).await });
            if let Ok(providers) = task.await {
                mode.state.borrow_mut().login_providers = providers.iter().map(AuthSelectorProvider::from).collect();
            }
        });
    }

    pub(super) fn login_completions(&self, prefix: &str) -> Option<Vec<AutocompleteItem>> {
        let providers = self.state.borrow().login_providers.clone();
        let options = login_completion_options(&providers);
        let searchable: Vec<(String, AuthSelectorProvider, Vec<AuthType>)> = options
            .into_iter()
            .map(|(provider, types)| {
                let labels: Vec<String> = types
                    .iter()
                    .map(|t| {
                        format!(
                            "{} {}",
                            auth_type_key(*t),
                            format_auth_selector_provider_type(*t, provider.subscription)
                        )
                    })
                    .collect();
                (format!("{} {} {}", provider.id, provider.name, labels.join(" ")), provider, types)
            })
            .collect();
        let filtered = fuzzy_filter(searchable, prefix, |(text, _, _)| text.as_str());
        let items: Vec<AutocompleteItem> = filtered
            .into_iter()
            .map(|(_, provider, types)| {
                let labels: Vec<&str> =
                    types.iter().map(|t| format_auth_selector_provider_type(*t, provider.subscription)).collect();
                let labels = labels.join("/");
                let description =
                    if provider.name == provider.id { labels } else { format!("{} · {labels}", provider.name) };
                AutocompleteItem::new(provider.id.clone(), provider.id.clone()).with_description(description)
            })
            .collect();
        (!items.is_empty()).then_some(items)
    }

    async fn login_options(&self, auth_type: Option<AuthType>) -> Vec<AuthSelectorProvider> {
        let runtime = self.session().model_runtime().clone();
        let task = tokio::spawn(async move { runtime.login_providers(auth_type).await });
        task.await.unwrap_or_default().iter().map(AuthSelectorProvider::from).collect()
    }

    /// `/login [provider]`.
    pub(super) fn handle_login_command(&self, reference: Option<String>) {
        self.spawn(move |mode| async move {
            let Some(reference) = reference.filter(|reference| !reference.trim().is_empty()) else {
                return mode.show_login_auth_type_selector(None).await;
            };
            let wanted = reference.trim().to_lowercase();
            let matches: Vec<AuthSelectorProvider> = mode
                .login_options(None)
                .await
                .into_iter()
                .filter(|provider| provider.id.to_lowercase() == wanted || provider.name.to_lowercase() == wanted)
                .collect();
            if matches.len() == 1 {
                return mode.show_login_dialog(matches[0].clone(), None);
            }
            if matches.len() > 1 && matches.iter().all(|provider| provider.id == matches[0].id) {
                return mode.show_login_auth_type_selector(Some(matches)).await;
            }
            mode.show_login_provider_selector(None, Some(reference)).await;
        });
    }

    async fn show_login_auth_type_selector(self: &Rc<Self>, providers: Option<Vec<AuthSelectorProvider>>) {
        let radius = match providers {
            Some(_) => None,
            None => self.login_options(Some(AuthType::OAuth)).await.into_iter().find(|p| p.id == RADIUS_PROVIDER_ID),
        };
        let oauth_label = providers
            .as_ref()
            .and_then(|providers| providers.iter().find(|p| p.auth_type == AuthType::OAuth))
            .and_then(|provider| provider.method_name.clone());
        let menu = LoginMethodMenu::new(providers.as_deref(), radius.as_ref(), oauth_label.as_deref());
        if menu.options.is_empty() {
            return self.show_status("No login methods available.");
        }
        if let Some(providers) = providers.as_ref().filter(|_| menu.options.len() == 1) {
            if let Some(provider) = providers.first() {
                self.show_login_dialog(provider.clone(), None);
            }
            return;
        }
        let tui = self.tui();
        let weak = self.weak.clone();
        self.selector.show(Box::new(move |done| {
            let (cancel_done, select_done) = (done.clone(), done);
            let cancel_weak = weak.clone();
            let choices = menu.clone();
            let on_select = move |option: &str| {
                select_done();
                let Some(mode) = weak.upgrade() else { return };
                let reopen_providers = providers.clone();
                let back_weak = mode.weak.clone();
                let back: Rc<dyn Fn()> = Rc::new(move || {
                    let providers = reopen_providers.clone();
                    if let Some(mode) = back_weak.upgrade() {
                        mode.spawn(move |mode| async move { mode.show_login_auth_type_selector(providers).await });
                    }
                });
                match choices.choice(option) {
                    LoginMethodChoice::Radius => {
                        if let Some(radius) = radius.clone() {
                            mode.show_login_dialog(radius, Some(back));
                        }
                    }
                    LoginMethodChoice::AuthType(auth_type) => match &providers {
                        Some(providers) => {
                            if let Some(provider) = providers.iter().find(|p| p.auth_type == auth_type) {
                                mode.show_login_dialog(provider.clone(), Some(back));
                            }
                        }
                        None => mode.spawn(move |mode| async move {
                            mode.show_login_provider_selector(Some(auth_type), None).await
                        }),
                    },
                }
            };
            let on_cancel = move || {
                cancel_done();
                if let Some(mode) = cancel_weak.upgrade() {
                    mode.request_render();
                }
            };
            match menu.radius.clone() {
                Some(radius_option) => {
                    let menu =
                        RadiusLoginMenu::new(&tui, &menu.title, menu.options, radius_option, on_select, on_cancel);
                    let dispose = menu.disposer();
                    SelectorSpec::new(component(menu)).with_dispose(dispose)
                }
                None => {
                    let selector = ExtensionSelector::new(
                        &menu.title,
                        menu.options,
                        on_select,
                        on_cancel,
                        ExtensionSelectorOptions::default(),
                    );
                    let dispose = selector.disposer();
                    SelectorSpec::new(component(selector)).with_dispose(dispose)
                }
            }
        }));
    }

    async fn show_login_provider_selector(self: &Rc<Self>, auth_type: Option<AuthType>, search: Option<String>) {
        let providers = self.login_options(auth_type).await;
        if providers.is_empty() {
            return self.show_status(no_login_providers_message(auth_type));
        }
        let weak = self.weak.clone();
        self.selector.show(Box::new(move |done| {
            let (select_done, cancel_done) = (done.clone(), done);
            let (select_weak, cancel_weak) = (weak.clone(), weak);
            let options = providers.clone();
            let reopen_search = search.clone();
            let selector = OAuthSelector::new(
                AuthSelectorMode::Login,
                providers,
                move |provider_id, selected| {
                    select_done();
                    let Some(mode) = select_weak.upgrade() else { return };
                    let Some(provider) = options.iter().find(|p| p.id == provider_id && p.auth_type == selected) else {
                        return;
                    };
                    let (back_weak, search) = (mode.weak.clone(), reopen_search.clone());
                    let back: Rc<dyn Fn()> = Rc::new(move || {
                        let search = search.clone();
                        if let Some(mode) = back_weak.upgrade() {
                            mode.spawn(move |mode| async move {
                                mode.show_login_provider_selector(auth_type, search).await
                            });
                        }
                    });
                    mode.show_login_dialog(provider.clone(), Some(back));
                },
                move || {
                    cancel_done();
                    let Some(mode) = cancel_weak.upgrade() else { return };
                    match auth_type {
                        Some(_) => mode.spawn(|mode| async move { mode.show_login_auth_type_selector(None).await }),
                        None => mode.request_render(),
                    }
                },
                search.as_deref(),
            );
            SelectorSpec::new(component(selector))
        }));
    }

    /// The login dialog in the editor slot, running the provider's flow until it ends.
    fn show_login_dialog(&self, provider: AuthSelectorProvider, on_back: OnBack) {
        let previous_model = self.session().model();
        let dialog =
            Rc::new(RefCell::new(LoginDialog::new(&self.tui(), &provider.id, |_, _| {}, Some(&provider.name), None)));
        if provider.auth_type == AuthType::ApiKey {
            let doc = crate::packages::package_dir().join("docs").join("providers.md");
            if let Some(lines) = api_key_login_details(&provider.id, &doc.display().to_string()) {
                dialog.borrow_mut().show_details(&lines);
            }
        }
        let close = self.mount_view(dialog.clone());
        self.set_blocked_status("login", Some((BlockedKind::Auth, format!("Log in to {}", provider.name))));
        let settings = self.settings();
        let device_id: Arc<dyn Fn() -> String + Send + Sync> = Arc::new(move || settings.get_or_create_device_id());
        let options = LoginOptions { device_id: Some(device_id), agent_name: None };
        let runtime = self.session().model_runtime().clone();
        self.spawn(move |mode| async move {
            let result = run_dialog_login(&dialog, &runtime, &provider.id, provider.auth_type, &options).await;
            mode.set_blocked_status("login", None);
            close();
            match result {
                Ok(_) => {
                    mode.complete_provider_authentication(&provider, previous_model).await;
                    if provider.id == RADIUS_PROVIDER_ID && provider.auth_type == AuthType::OAuth {
                        mode.offer_radius_mcp_server(&provider.id, &provider.name);
                    }
                }
                Err(AuthError::Cancelled) => {
                    if let Some(back) = on_back {
                        back();
                    }
                }
                Err(error) => {
                    let message = error.to_string();
                    match provider.auth_type {
                        AuthType::OAuth => mode.show_error(&format!("Failed to login to {}: {message}", provider.name)),
                        AuthType::ApiKey => {
                            mode.show_error(&format!("Failed to save API key for {}: {message}", provider.name))
                        }
                    }
                }
            }
        });
    }

    /// Picks the provider's default model when none was set, then reports where the credential went.
    async fn complete_provider_authentication(
        self: &Rc<Self>,
        provider: &AuthSelectorProvider,
        previous: Option<Model>,
    ) {
        let action = login_action_label(provider.auth_type, &provider.name);
        let session = self.session();
        let mut selected: Option<Model> = None;
        let mut selection_error = None;
        if previous.is_none() {
            let models: Vec<Model> =
                session.model_runtime().available_models().into_iter().filter(|m| m.provider == provider.id).collect();
            let default_id = default_model_for(&provider.id);
            if provider.id == LLAMA_PROVIDER_ID {
                selection_error = Some(llama_cpp_post_login_guidance(&action, models.len()));
            } else if default_id.is_none() {
                selection_error = Some(format!(
                    "{action}, but no default model is configured for provider \"{}\". Use /model to select a model.",
                    provider.id
                ));
            } else if models.is_empty() {
                selection_error = Some(format!(
                    "{action}, but no models are available for that provider. Use /model to select a model."
                ));
            } else {
                let default_id = default_id.unwrap_or_default();
                let found = models.iter().find(|model| model.id == default_id).cloned();
                let candidate = found.or_else(|| (provider.id == RADIUS_PROVIDER_ID).then(|| models[0].clone()));
                match candidate {
                    None => {
                        selection_error = Some(format!(
                            "{action}, but its default model \"{default_id}\" is not available. Use /model to select a model."
                        ))
                    }
                    Some(model) => {
                        let (target, setter) = (model.clone(), session.clone());
                        match tokio::spawn(async move { setter.set_model(target, true).await }).await {
                            Ok(Ok(())) => selected = Some(model),
                            Ok(Err(error)) => {
                                selection_error = Some(format!(
                                    "{action}, but selecting its default model failed: {error}. Use /model to select a model."
                                ))
                            }
                            Err(error) => selection_error = Some(error.to_string()),
                        }
                    }
                }
            }
        }
        self.update_available_provider_count();
        self.footer.borrow_mut().invalidate();
        self.update_editor_border_color();
        self.refresh_login_providers();
        let auth_path = crate::config::auth_path(&self.agent_dir);
        let saved = format!("Credentials saved to {}", auth_path.display());
        match &selected {
            Some(model) => self.show_status(&format!("{action}. Selected {}. {saved}", model.id)),
            None => self.show_status(&format!("{action}. {saved}")),
        }
        match selection_error {
            Some(error) if selected.is_none() => self.show_error(&error),
            _ => self.maybe_warn_about_anthropic_subscription_auth(selected),
        }
    }

    /// Warns once when Anthropic requests use subscription auth.
    pub(super) fn maybe_warn_about_anthropic_subscription_auth(&self, model: Option<Model>) {
        if !self.settings().warn_anthropic_extra_usage() || self.state.borrow().anthropic_warning_shown {
            return;
        }
        let Some(model) = model.or_else(|| self.session().model()) else { return };
        if model.provider != "anthropic" {
            return;
        }
        let runtime = self.session().model_runtime().clone();
        self.spawn(move |mode| async move {
            let check = async move {
                if let Ok(Some(check)) = runtime.resolver().check("anthropic").await
                    && check.auth_type == AuthType::OAuth
                {
                    return true;
                }
                runtime.api_key_for(&model.provider).await.is_some_and(|key| key.starts_with("sk-ant-oat"))
            };
            if tokio::spawn(check).await.unwrap_or(false) && !mode.state.borrow().anthropic_warning_shown {
                mode.state.borrow_mut().anthropic_warning_shown = true;
                mode.show_warning(ANTHROPIC_SUBSCRIPTION_AUTH_WARNING);
            }
        });
    }

    /// `/logout`: remove a credential saved by `/login`.
    pub(super) fn show_logout_selector(&self) {
        let runtime = self.session().model_runtime().clone();
        self.spawn(move |mode| async move {
            let listing = runtime.clone();
            let providers = match tokio::spawn(async move { listing.logout_providers().await }).await {
                Ok(Ok(providers)) => providers,
                Ok(Err(error)) => return mode.show_error(&format!("Could not read stored credentials: {error}")),
                Err(error) => return mode.show_error(&format!("Could not read stored credentials: {error}")),
            };
            if providers.is_empty() {
                return mode.show_status(NO_STORED_CREDENTIALS);
            }
            let weak = mode.weak.clone();
            mode.selector.show(Box::new(move |done| {
                let (select_done, cancel_done) = (done.clone(), done);
                let (select_weak, cancel_weak) = (weak.clone(), weak);
                let options = providers.clone();
                let selector = OAuthSelector::new(
                    AuthSelectorMode::Logout,
                    providers.iter().map(AuthSelectorProvider::from).collect(),
                    move |provider_id, _| {
                        select_done();
                        let Some(mode) = select_weak.upgrade() else { return };
                        let Some(provider) = options.iter().find(|p| p.id == provider_id).cloned() else { return };
                        let runtime = runtime.clone();
                        mode.spawn(move |mode| async move {
                            let id = provider.id.clone();
                            match tokio::spawn(async move { runtime.logout(&id).await }).await {
                                Ok(Ok(())) => {
                                    mode.update_available_provider_count();
                                    mode.refresh_login_providers();
                                    mode.show_status(&logout_message(&provider));
                                }
                                Ok(Err(error)) => mode.show_error(&format!("Logout failed: {error}")),
                                Err(error) => mode.show_error(&format!("Logout failed: {error}")),
                            }
                        });
                    },
                    move || {
                        cancel_done();
                        if let Some(mode) = cancel_weak.upgrade() {
                            mode.request_render();
                        }
                    },
                    None,
                );
                SelectorSpec::new(component(selector))
            }));
        });
    }
}
