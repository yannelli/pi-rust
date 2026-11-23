//! The share service question (pi-rust): the first interactive `/share` asks whether to keep
//! upstream pi's pi.dev services, the default of a drop-in replacement, or to share to a
//! pi-rust.com account. Nothing asks at startup, and other modes keep the pi.dev default.

use crate::settings::SettingsManager;
use crate::settings::schema::{SessionShareDestination, SessionShareViewer};

pub const SHARE_SERVICE_TITLE: &str = "Where should /share send your sessions?\nChange it later in /settings.";
pub const SHARE_SERVICE_PI_DEV: &str = "pi.dev: as upstream pi (a Radius account, or GitHub gists viewed on pi.dev)";
pub const SHARE_SERVICE_PI_RUST: &str = "pi-rust.com: your pi-rust.com account";

/// The service the user picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareService {
    PiDev,
    PiRust,
}

/// The options in display order: pi.dev, the default, first.
pub fn share_service_options() -> Vec<String> {
    vec![SHARE_SERVICE_PI_DEV.to_string(), SHARE_SERVICE_PI_RUST.to_string()]
}

/// The service of a selected option; `None` when the question was dismissed.
pub fn parse_share_service(option: Option<&str>) -> Option<ShareService> {
    match option? {
        SHARE_SERVICE_PI_DEV => Some(ShareService::PiDev),
        SHARE_SERVICE_PI_RUST => Some(ShareService::PiRust),
        _ => None,
    }
}

/// Whether `/share` asks first: until `sessionShareViewer` or `sessionShareDestination` is set.
pub fn should_ask_share_service(settings: &SettingsManager) -> bool {
    !settings.session_share_service_chosen()
}

/// Saves the answer. pi.dev keeps upstream's `auto` (Radius when signed in, else a gist) with gist
/// links on pi.dev. pi-rust.com shares to the pi-rust.com account (sign-in on the first `/share`)
/// and opens gist links there.
pub fn apply_share_service(settings: &SettingsManager, service: ShareService) {
    let (viewer, destination) = match service {
        ShareService::PiDev => (SessionShareViewer::PiDev, SessionShareDestination::Auto),
        ShareService::PiRust => (SessionShareViewer::PiRust, SessionShareDestination::PiRust),
    };
    settings.set_session_share_viewer(viewer);
    settings.set_session_share_destination(destination);
    settings.flush();
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn asks_until_either_share_setting_is_set() {
        assert!(should_ask_share_service(&SettingsManager::in_memory(json!({}))));
        assert!(!should_ask_share_service(&SettingsManager::in_memory(json!({"sessionShareViewer": "pi.dev"}))));
        assert!(!should_ask_share_service(&SettingsManager::in_memory(json!({"sessionShareDestination": "gist"}))));
        let settings = SettingsManager::in_memory(json!({}));
        apply_share_service(&settings, ShareService::PiDev);
        assert!(!should_ask_share_service(&settings));
    }

    #[test]
    fn each_answer_sets_the_viewer_and_destination() {
        let settings = SettingsManager::in_memory(json!({}));
        apply_share_service(&settings, ShareService::PiRust);
        assert_eq!(settings.session_share_viewer(), SessionShareViewer::PiRust);
        assert_eq!(settings.session_share_destination(), SessionShareDestination::PiRust);
        apply_share_service(&settings, ShareService::PiDev);
        assert_eq!(settings.session_share_viewer(), SessionShareViewer::PiDev);
        assert_eq!(settings.session_share_destination(), SessionShareDestination::Auto);
        assert_eq!(parse_share_service(Some(SHARE_SERVICE_PI_RUST)), Some(ShareService::PiRust));
        assert_eq!(parse_share_service(None), None);
        assert_eq!(share_service_options(), [SHARE_SERVICE_PI_DEV, SHARE_SERVICE_PI_RUST]);
    }
}
