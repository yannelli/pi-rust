//! The startup offer to install the marketplace extensions that replaced the status line and the
//! iTerm2 integration, and the hint for `/statusline` and `/terminal-setup` without them.

use std::sync::Arc;

use super::mode::InteractiveMode;
use crate::agent_session::BUILTIN_EXTENSIONS;
use crate::packages::moved_features::{MigrationChoices, MovedFeature, missing_features};
use crate::packages::{Marketplace, PackageManager, PackageManagerOptions};

const INSTALL: &str = "Install";
const NOT_NOW: &str = "Not now";
const DO_NOT_ASK: &str = "Do not ask again";

/// Install the extension of `feature` and add its source to the global settings.
async fn install(manager: &PackageManager, marketplace: &Marketplace, feature: MovedFeature) -> anyhow::Result<()> {
    marketplace.install(feature.package(), None).await?;
    manager.add_source_to_settings(&feature.source(), false)?;
    let errors = manager.settings().drain_errors();
    anyhow::ensure!(
        errors.is_empty(),
        "{}",
        errors.iter().map(|error| error.diagnostic()).collect::<Vec<_>>().join("; ")
    );
    Ok(())
}

fn install_failed(feature: MovedFeature, error: &str) -> String {
    let error = error.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("Could not install {}: {error}. Install it with `{}`.", feature.source(), feature.install_command())
}

impl InteractiveMode {
    /// `/statusline` or `/terminal-setup` when no extension registers it: the install hint.
    pub(super) fn moved_feature_hint(&self, text: &str) -> Option<String> {
        let feature = MovedFeature::from_slash_command(text)?;
        let registered = self
            .session()
            .extension_runner()
            .is_some_and(|runner| runner.registry().command(feature.command()).is_some());
        (!registered).then(|| feature.hint())
    }

    /// At startup: offer the extensions of features that legacy settings turn on.
    pub(super) fn check_moved_features(&self) {
        let session = self.session();
        let mut options = PackageManagerOptions::new(session.cwd(), &self.agent_dir, self.settings());
        options.builtin_extensions = BUILTIN_EXTENSIONS.iter().map(|name| (*name).into()).collect();
        let manager = PackageManager::new(options);
        let marketplace = manager.marketplace();
        self.offer_moved_features(manager, marketplace);
    }

    /// Ask about each missing extension that was not dismissed; reload after an install.
    pub(super) fn offer_moved_features(&self, manager: PackageManager, marketplace: Marketplace) {
        let choices = MigrationChoices::load(&self.agent_dir);
        let features: Vec<MovedFeature> = missing_features(&self.settings(), &marketplace)
            .into_iter()
            .filter(|feature| !choices.is_dismissed(*feature))
            .collect();
        if features.is_empty() {
            return;
        }
        let agent_dir = self.agent_dir.clone();
        let backend = Arc::new((manager, marketplace));
        self.spawn(move |mode| async move {
            let mut installed = false;
            for feature in features {
                let title = format!("{} is now an extension. Install {}?", feature.title(), feature.source());
                let options = [INSTALL, NOT_NOW, DO_NOT_ASK].map(String::from).to_vec();
                let answer = mode.ask_choice(&title, options).await.ok().flatten();
                match answer.as_deref() {
                    Some(INSTALL) => {
                        let worker = backend.clone();
                        let result = tokio::spawn(async move { install(&worker.0, &worker.1, feature).await }).await;
                        match result {
                            Ok(Ok(())) => installed = true,
                            Ok(Err(error)) => mode.show_warning(&install_failed(feature, &format!("{error:#}"))),
                            Err(error) => mode.show_warning(&install_failed(feature, &error.to_string())),
                        }
                    }
                    Some(DO_NOT_ASK) => {
                        if let Err(error) = MigrationChoices::dismiss(&agent_dir, feature) {
                            mode.show_warning(&format!("Could not save the choice for {}: {error}", feature.source()));
                        }
                    }
                    _ => {}
                }
            }
            if installed {
                mode.handle_reload_command();
            }
        });
    }
}

#[cfg(test)]
#[path = "../../tests/interactive/moved_features.rs"]
mod tests;
