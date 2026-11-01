//! Built-in OAuth login flows and the helpers they share: PKCE, a loopback callback
//! server raced against a pasted redirect URL, device-code polling, and the result page.

use std::sync::Arc;

use crate::auth::OAuthAuth;

mod anthropic;
mod antigravity_acp;
mod browser;
mod callback_server;
mod cursor;
mod device_code;
mod github_copilot;
mod github_copilot_models;
mod http;
pub(crate) mod interact;
mod kimi_coding;
mod kimi_region;
mod loopback;
mod meta;
mod openai_chatgpt;
mod openai_codex;
mod openrouter;
mod page;
mod pkce;
mod radius;
mod radius_config;
mod xai;

#[cfg(test)]
mod tests;

pub use anthropic::AnthropicOAuth;
pub use antigravity_acp::{ANTIGRAVITY_ACP_MARKER, AntigravityAcpOAuth};
pub use browser::{browser_command, open_browser};
pub use callback_server::{
    CallbackOrManual, CallbackServer, CallbackServerOptions, CompleteFn, start_callback_server,
    wait_for_callback_or_manual_input,
};
pub use cursor::CursorOAuth;
pub use device_code::{DeviceCodePollOptions, DevicePoll, cancellable_sleep, poll_device_code_flow};
pub use github_copilot::GitHubCopilotOAuth;
pub use github_copilot_models::github_copilot_base_url;
pub(crate) use github_copilot_models::{
    COPILOT_API_VERSION, COPILOT_HEADERS, INDIVIDUAL_BASE_URL as COPILOT_INDIVIDUAL_BASE_URL,
    parse_catalog as parse_copilot_catalog,
};
pub use kimi_coding::KimiCodingOAuth;
pub use meta::MetaOAuth;
pub use openai_chatgpt::OpenAIChatGPTOAuth;
pub use openai_codex::OpenAICodexOAuth;
pub use openrouter::OpenRouterOAuth;
pub use page::{oauth_error_html, oauth_success_html};
pub use pkce::{Pkce, base64url_encode, generate_pkce, pkce_challenge, random_uuid};
pub use radius::RadiusOAuth;
pub use radius_config::{
    DEFAULT_RADIUS_GATEWAY, RadiusGatewayConfig, load_radius_gateway_config, normalize_radius_gateway_url,
    radius_credential_config, radius_models, radius_models_from_config,
};
pub use xai::XaiOAuth;

/// Provider ids with a built-in OAuth flow, in upstream's provider order.
/// The providers only pi-rust has follow them.
pub const BUILTIN_OAUTH_PROVIDER_IDS: &[&str] = &[
    "anthropic",
    "github-copilot",
    "kimi-coding",
    "meta",
    "openai",
    "openai-codex",
    "openrouter",
    "radius",
    "xai",
    "cursor",
    "google-antigravity",
];

/// A Radius login for a configured gateway, as upstream's `createRadiusOAuth({ name, gateway })`.
pub fn radius_oauth(name: &str, gateway: &str) -> Arc<dyn OAuthAuth> {
    Arc::new(RadiusOAuth::new(name, gateway))
}

/// The built-in OAuth flow for a provider id, as upstream's provider definitions wire them.
/// `openai` is Sign in with ChatGPT; `openai-codex` is the ChatGPT Plus/Pro Codex login.
pub fn builtin_oauth(provider_id: &str) -> Option<Arc<dyn OAuthAuth>> {
    let oauth: Arc<dyn OAuthAuth> = match provider_id {
        "anthropic" => Arc::new(AnthropicOAuth::default()),
        "github-copilot" => Arc::new(GitHubCopilotOAuth::default()),
        "kimi-coding" => Arc::new(KimiCodingOAuth::default()),
        "meta" => Arc::new(MetaOAuth::default()),
        "openai" => Arc::new(OpenAIChatGPTOAuth::default()),
        "openai-codex" => Arc::new(OpenAICodexOAuth::default()),
        "openrouter" => Arc::new(OpenRouterOAuth::default()),
        "radius" => radius_oauth("Radius", DEFAULT_RADIUS_GATEWAY),
        "xai" => Arc::new(XaiOAuth::default()),
        "cursor" => Arc::new(CursorOAuth::default()),
        "google-antigravity" => Arc::new(AntigravityAcpOAuth),
        _ => return None,
    };
    Some(oauth)
}

/// Every built-in OAuth flow keyed by provider id.
pub fn builtin_oauth_providers() -> Vec<(String, Arc<dyn OAuthAuth>)> {
    BUILTIN_OAUTH_PROVIDER_IDS
        .iter()
        .filter_map(|id| builtin_oauth(id).map(|oauth| ((*id).to_string(), oauth)))
        .collect()
}
