//! Former pi-rust built-ins that are marketplace extensions now. A "do not ask again" choice is
//! kept in `<agentDir>/marketplace/migrations.json`, so `settings.json` stays upstream-compatible.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::marketplace::Marketplace;
use crate::resources::brand::APP_NAME;
use crate::settings::SettingsManager;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MovedFeature {
    StatusLine,
    ITerm2,
}

impl MovedFeature {
    pub const ALL: [Self; 2] = [Self::StatusLine, Self::ITerm2];

    /// The marketplace extension name.
    pub fn package(self) -> &'static str {
        match self {
            Self::StatusLine => "statusline",
            Self::ITerm2 => "iterm2",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::StatusLine => "The status line",
            Self::ITerm2 => "The iTerm2 integration",
        }
    }

    /// The legacy settings key the extension reads.
    pub fn setting(self) -> &'static str {
        match self {
            Self::StatusLine => "statusLine",
            Self::ITerm2 => "terminal.iterm2",
        }
    }

    /// The slash command the extension registers.
    pub fn command(self) -> &'static str {
        match self {
            Self::StatusLine => "statusline",
            Self::ITerm2 => "terminal-setup",
        }
    }

    pub fn source(self) -> String {
        format!("marketplace:{}", self.package())
    }

    pub fn install_command(self) -> String {
        format!("{APP_NAME} install {}", self.source())
    }

    /// The one-line hint for a user without the extension.
    pub fn hint(self) -> String {
        format!("{} is now an extension. Install it with `{}`.", self.title(), self.install_command())
    }

    /// The legacy setting turns the feature on.
    pub fn enabled(self, settings: &SettingsManager) -> bool {
        match self {
            Self::StatusLine => settings.legacy_status_line_enabled(),
            Self::ITerm2 => settings.legacy_iterm2_enabled(),
        }
    }

    pub fn installed(self, marketplace: &Marketplace) -> bool {
        matches!(marketplace.installed_package(self.package()), Ok(Some(_)))
    }

    /// The feature whose command `text` runs, such as `/statusline compact`.
    pub fn from_slash_command(text: &str) -> Option<Self> {
        let name = text.strip_prefix('/')?.split_whitespace().next()?;
        Self::ALL.into_iter().find(|feature| feature.command() == name)
    }
}

/// Features the settings turn on whose extension is not installed.
pub fn missing_features(settings: &SettingsManager, marketplace: &Marketplace) -> Vec<MovedFeature> {
    MovedFeature::ALL
        .into_iter()
        .filter(|feature| feature.enabled(settings) && !feature.installed(marketplace))
        .collect()
}

/// `pi doctor` warnings for [`missing_features`].
pub fn doctor_warnings(settings: &SettingsManager, marketplace: &Marketplace) -> Vec<String> {
    missing_features(settings, marketplace)
        .into_iter()
        .map(|feature| {
            format!(
                "`{}` is set, but {} is not installed. Run `{}`.",
                feature.setting(),
                feature.source(),
                feature.install_command()
            )
        })
        .collect()
}

/// The migration choices in `<agentDir>/marketplace/migrations.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationChoices {
    /// Extension names the user does not want to be asked about again.
    #[serde(default)]
    pub dismissed: BTreeSet<String>,
}

impl MigrationChoices {
    pub fn path(agent_dir: &Path) -> PathBuf {
        agent_dir.join("marketplace").join("migrations.json")
    }

    /// The saved choices; none when the file is missing or not valid.
    pub fn load(agent_dir: &Path) -> Self {
        std::fs::read(Self::path(agent_dir))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn is_dismissed(&self, feature: MovedFeature) -> bool {
        self.dismissed.contains(feature.package())
    }

    /// Save "do not ask again" for `feature`.
    pub fn dismiss(agent_dir: &Path, feature: MovedFeature) -> std::io::Result<()> {
        let mut choices = Self::load(agent_dir);
        choices.dismissed.insert(feature.package().to_string());
        let path = Self::path(agent_dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_vec_pretty(&choices).map_err(std::io::Error::other)?;
        std::fs::write(path, body)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::platform::test_http::client;

    #[test]
    fn legacy_settings_turn_features_on() {
        let enabled = |settings: serde_json::Value| {
            let settings = SettingsManager::in_memory(settings);
            MovedFeature::ALL.into_iter().filter(|feature| feature.enabled(&settings)).collect::<Vec<_>>()
        };
        assert_eq!(enabled(json!({})), []);
        assert_eq!(enabled(json!({"statusLine": {"preset": "pills"}})), [MovedFeature::StatusLine]);
        assert_eq!(enabled(json!({"statusLine": {"type": "off"}})), []);
        assert_eq!(enabled(json!({"terminal": {"iterm2": true}})), [MovedFeature::ITerm2]);
        assert_eq!(enabled(json!({"terminal": {"iterm2": {"badge": false}}})), [MovedFeature::ITerm2]);
        assert_eq!(enabled(json!({"terminal": {"iterm2": false}})), []);
    }

    #[test]
    fn slash_commands_map_to_features() {
        assert_eq!(MovedFeature::from_slash_command("/statusline"), Some(MovedFeature::StatusLine));
        assert_eq!(MovedFeature::from_slash_command("/statusline compact"), Some(MovedFeature::StatusLine));
        assert_eq!(MovedFeature::from_slash_command("/terminal-setup"), Some(MovedFeature::ITerm2));
        assert_eq!(MovedFeature::from_slash_command("/statuslines"), None);
        assert_eq!(MovedFeature::from_slash_command("statusline"), None);
        assert!(MovedFeature::StatusLine.hint().contains("`pi install marketplace:statusline`"));
    }

    #[test]
    fn dismissed_choices_persist_and_bad_files_reset() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!MigrationChoices::load(dir.path()).is_dismissed(MovedFeature::ITerm2));
        MigrationChoices::dismiss(dir.path(), MovedFeature::ITerm2).unwrap();
        MigrationChoices::dismiss(dir.path(), MovedFeature::StatusLine).unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(MigrationChoices::path(dir.path())).unwrap()).unwrap();
        assert_eq!(saved, json!({"dismissed": ["iterm2", "statusline"]}));
        std::fs::write(MigrationChoices::path(dir.path()), "not json").unwrap();
        assert_eq!(MigrationChoices::load(dir.path()), MigrationChoices::default());
    }

    #[test]
    fn doctor_warns_only_about_missing_extensions() {
        let dir = tempfile::tempdir().unwrap();
        let marketplace = Marketplace::new(dir.path().to_path_buf(), client(), true);
        let settings = SettingsManager::in_memory(json!({"statusLine": {}, "terminal": {"iterm2": true}}));
        let warnings = doctor_warnings(&settings, &marketplace);
        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].contains("Run `pi install marketplace:statusline`"));
        assert!(doctor_warnings(&SettingsManager::in_memory(json!({})), &marketplace).is_empty());
    }
}
