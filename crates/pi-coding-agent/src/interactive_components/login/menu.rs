//! The `/login` method menu and the API key and ambient credential dialogs
//! (upstream `interactive-mode.ts` `showLoginAuthTypeSelector`, `showApiKeyLoginDialog`, and
//! `showAmbientAuthDialog`).

use pi_ai::auth::AuthType;
use pi_tui::Tui;

use super::{AuthSelectorProvider, LoginDialog, RadiusOption, format_auth_selector_provider_status};
use crate::interactive_components::common::fg;
use crate::resources::brand::APP_NAME;
use crate::theme::ThemeColor;

pub const API_KEY_LOGIN_LABEL: &str = "Sign in with an API key";
pub const ACCOUNT_LOGIN_LABEL: &str = "Sign in with an account";

/// The method menu: what to show and how to map a chosen label back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginMethodMenu {
    pub title: String,
    pub options: Vec<String>,
    /// The OAuth option's label (the provider's login label or [`ACCOUNT_LOGIN_LABEL`]).
    pub subscription_label: String,
    /// Set when the top-level menu offers Radius as its last option; use a `RadiusLoginMenu`.
    pub radius: Option<RadiusOption>,
}

/// What a chosen menu label means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginMethodChoice {
    Radius,
    AuthType(AuthType),
}

impl LoginMethodMenu {
    /// `provider_options` limits the menu to one provider's methods (`/login <provider>`);
    /// otherwise the menu is top level and `radius` (the Radius OAuth option) is appended.
    pub fn new(
        provider_options: Option<&[AuthSelectorProvider]>,
        radius: Option<&AuthSelectorProvider>,
        oauth_login_label: Option<&str>,
    ) -> Self {
        let radius = if provider_options.is_some() { None } else { radius };
        let radius = radius.map(|radius| {
            let text = format!("Sign in with {}", radius.name);
            RadiusOption { label: format!("{text}{}", format_auth_selector_provider_status(radius)), text }
        });
        let subscription_label = oauth_login_label.unwrap_or(ACCOUNT_LOGIN_LABEL).to_string();
        let has = |auth_type| provider_options.is_none_or(|options| options.iter().any(|p| p.auth_type == auth_type));
        let mut options = Vec::new();
        if has(AuthType::OAuth) {
            options.push(subscription_label.clone());
        }
        if has(AuthType::ApiKey) {
            options.push(API_KEY_LOGIN_LABEL.to_string());
        }
        if let Some(radius) = &radius {
            options.push(radius.label.clone());
        }
        let title = match provider_options.and_then(|options| options.first()) {
            Some(provider) => format!("Select authentication method for {}:", provider.name),
            None => "Select authentication method:".to_string(),
        };
        Self { title, options, subscription_label, radius }
    }

    /// Maps a selected label to Radius or an auth type.
    pub fn choice(&self, option: &str) -> LoginMethodChoice {
        if self.radius.as_ref().is_some_and(|radius| radius.label == option) {
            LoginMethodChoice::Radius
        } else if option == self.subscription_label {
            LoginMethodChoice::AuthType(AuthType::OAuth)
        } else {
            LoginMethodChoice::AuthType(AuthType::ApiKey)
        }
    }
}

/// The status shown when the selector would be empty.
pub fn no_login_providers_message(auth_type: Option<AuthType>) -> &'static str {
    match auth_type {
        Some(AuthType::OAuth) => "No account providers available.",
        Some(AuthType::ApiKey) => "No API key providers available.",
        None => "No login providers available.",
    }
}

/// Lines shown before the API key prompt; only Amazon Bedrock has them.
pub fn api_key_login_details(provider_id: &str, providers_doc: &str) -> Option<Vec<String>> {
    (provider_id == "amazon-bedrock").then(|| {
        vec![
            fg(ThemeColor::Text, "You can also use an AWS profile, IAM keys, or role-based credentials."),
            fg(ThemeColor::Muted, "See:"),
            fg(ThemeColor::Accent, &format!("  {providers_doc}")),
        ]
    })
}

/// The dialog for a provider whose credentials come from outside (`<name> setup`). Closing it
/// (the cancel key) calls `on_close`.
pub fn ambient_auth_dialog(
    tui: &Tui,
    provider_id: &str,
    provider_name: &str,
    method_name: Option<&str>,
    mut on_close: impl FnMut() + 'static,
) -> LoginDialog {
    let title = format!("{provider_name} setup");
    let mut dialog = LoginDialog::new(tui, provider_id, move |_, _| on_close(), Some(provider_name), Some(&title));
    let method = method_name.unwrap_or("Authentication");
    dialog.show_info(&format!("{method} is configured outside {APP_NAME}."), &[], true);
    dialog
}

/// The status after a successful login or API key save, before the model selection suffix.
pub fn login_action_label(auth_type: AuthType, provider_name: &str) -> String {
    match auth_type {
        AuthType::OAuth => format!("Logged in to {provider_name}"),
        AuthType::ApiKey => format!("Saved API key for {provider_name}"),
    }
}
