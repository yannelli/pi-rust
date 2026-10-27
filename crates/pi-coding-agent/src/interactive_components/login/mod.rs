//! Login UI: the provider selector, the Radius menu, the login dialog with its auth URL, and
//! the bridge that runs a login flow (OAuth, API key, llama.cpp URL) through the dialog.

mod auth_url;
mod dialog;
mod interaction;
mod llama_cpp;
mod menu;
mod provider_selector;
mod radius_menu;

pub use auth_url::{AuthUrl, BrowserOpenFn, ClipboardCopyFn, system_browser, system_clipboard};
pub use dialog::{DeviceCodeInfo, LoginDialog};
pub use interaction::{DialogInteraction, RADIUS_LOGIN_INTRO, dialog_interaction, notify_dialog, run_dialog_login};
pub use llama_cpp::llama_cpp_post_login_guidance;
pub use menu::{
    ACCOUNT_LOGIN_LABEL, API_KEY_LOGIN_LABEL, LoginMethodChoice, LoginMethodMenu, ambient_auth_dialog,
    api_key_login_details, login_action_label, no_login_providers_message,
};
pub use provider_selector::{
    AuthSelectorMode, AuthSelectorProvider, OAuthSelector, format_auth_selector_provider_status,
    format_auth_selector_provider_type,
};
pub use radius_menu::{RadiusLoginMenu, RadiusOption, radius_shimmer};
