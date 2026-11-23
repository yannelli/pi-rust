//! The one-time `/share` service question (pi-rust): keep upstream pi's pi.dev services, the
//! default of a drop-in replacement, or share to a pi-rust.com account. Asked in interactive mode
//! until the user has chosen, so people who installed before the question existed see it once too.

use crate::settings::SettingsManager;
use crate::settings::schema::{SessionShareDestination, SessionShareViewer};

pub const SHARE_SERVICE_TITLE: &str = "Where should /share send your sessions?\nChange it later in /settings.";
pub const SHARE_SERVICE_PI_DEV: &str = "pi.dev: as upstream pi (a Radius account, or GitHub gists viewed on pi.dev)";
pub const SHARE_SERVICE_PI_RUST: &str = "pi-rust.com: your pi-rust.com account";

/// The service the user picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareService {
    PiRust,
    PiDev,
}

/// The options in display order: pi.dev, the default, first.
pub fn share_service_options() -> Vec<String> {
    vec![SHARE_SERVICE_PI_DEV.to_string(), SHARE_SERVICE_PI_RUST.to_string()]
}

/// The service of a selected option; `None` when the question was dismissed.
pub fn parse_share_service(option: Option<&str>) -> Option<ShareService> {
    match option? {
        SHARE_SERVICE_PI_RUST => Some(ShareService::PiRust),
        SHARE_SERVICE_PI_DEV => Some(ShareService::PiDev),
        _ => None,
    }
}

/// Whether to ask: the global settings have no `sessionShareViewer` yet, and no custom agent
/// directory is set (as for first-time setup, such runs are usually automated).
pub fn should_ask_share_service(settings: &SettingsManager, env: impl Fn(&str) -> Option<String>) -> bool {
    if env(&crate::resources::brand::agent_dir_env_var()).is_some_and(|dir| !dir.is_empty()) {
        return false;
    }
    !settings.session_share_viewer_chosen()
}

/// Saves the answer. pi.dev keeps upstream's `auto` (Radius when signed in, else a gist) with gist
/// links on pi.dev. pi-rust.com shares to the pi-rust.com account (sign-in on the first `/share`)
/// and opens gist links there. Dismissing records the pi.dev default, so it is asked once.
pub fn apply_share_service(settings: &SettingsManager, service: Option<ShareService>) {
    match service {
        Some(ShareService::PiRust) => {
            settings.set_session_share_viewer(SessionShareViewer::PiRust);
            settings.set_session_share_destination(SessionShareDestination::PiRust);
        }
        Some(ShareService::PiDev) => {
            settings.set_session_share_viewer(SessionShareViewer::PiDev);
            settings.set_session_share_destination(SessionShareDestination::Auto);
        }
        None => settings.set_session_share_viewer(SessionShareViewer::PiDev),
    }
    settings.flush();
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn asks_until_a_service_is_chosen() {
        let settings = SettingsManager::in_memory(json!({}));
        assert!(should_ask_share_service(&settings, no_env));
        let custom_dir =
            |name: &str| (name == crate::resources::brand::agent_dir_env_var()).then(|| "/tmp/agent".into());
        assert!(!should_ask_share_service(&settings, custom_dir));
        apply_share_service(&settings, None);
        assert!(!should_ask_share_service(&settings, no_env));
        assert_eq!(settings.session_share_viewer(), SessionShareViewer::PiDev);
        assert_eq!(settings.session_share_destination(), SessionShareDestination::Auto);
        let chosen = SettingsManager::in_memory(json!({"sessionShareViewer": "pi.dev"}));
        assert!(!should_ask_share_service(&chosen, no_env));
    }

    #[test]
    fn each_answer_sets_the_viewer_and_destination() {
        let settings = SettingsManager::in_memory(json!({}));
        apply_share_service(&settings, parse_share_service(Some(SHARE_SERVICE_PI_RUST)));
        assert_eq!(settings.session_share_viewer(), SessionShareViewer::PiRust);
        assert_eq!(settings.session_share_destination(), SessionShareDestination::PiRust);
        apply_share_service(&settings, parse_share_service(Some(SHARE_SERVICE_PI_DEV)));
        assert_eq!(settings.session_share_viewer(), SessionShareViewer::PiDev);
        assert_eq!(settings.session_share_destination(), SessionShareDestination::Auto);
        assert_eq!(parse_share_service(None), None);
        assert_eq!(share_service_options(), [SHARE_SERVICE_PI_DEV, SHARE_SERVICE_PI_RUST]);
    }
}
