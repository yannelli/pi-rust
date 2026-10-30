//! `install` and `remove` for every source kind, with progress events and the settings
//! updates of `pi install` and `pi remove`.

use anyhow::bail;

use super::settings_ops::package_source_text;
use super::source::{ParsedSource, parse_source};
use super::{PackageManager, ProgressAction};
use crate::resources::SourceScope;
use crate::settings::SettingsScope;

fn scope_for(local: bool) -> SourceScope {
    if local { SourceScope::Project } else { SourceScope::User }
}

impl PackageManager {
    pub fn marketplace(&self) -> super::marketplace::Marketplace {
        super::marketplace::Marketplace::new(self.agent_dir.clone(), self.http(), self.is_offline())
    }

    /// Install a source into user storage, or project storage with `local`. Local paths are
    /// never copied; installing one only checks that it exists.
    pub async fn install(&self, source: &str, local: bool) -> anyhow::Result<()> {
        let parsed = parse_source(source);
        let scope = scope_for(local);
        self.assert_project_trusted(scope)?;
        let operation = async {
            match &parsed {
                ParsedSource::Npm(npm) => self.install_npm(npm, scope, false).await,
                ParsedSource::Git(git) => self.install_git(git, scope).await,
                ParsedSource::Archive(archive) => self.install_archive(archive, scope, false).await,
                ParsedSource::Marketplace { name, tag } => {
                    self.marketplace().install(name, tag.as_deref()).await.map(|_| ())
                }
                ParsedSource::Local(local) => {
                    let resolved = self.resolve_input_path(&local.path);
                    if !resolved.exists() {
                        bail!("Path does not exist: {}", resolved.display());
                    }
                    Ok(())
                }
            }
        };
        self.with_progress(ProgressAction::Install, source, format!("Installing {source}..."), operation).await
    }

    /// `install`, then add the source to settings.
    pub async fn install_and_persist(&self, source: &str, local: bool) -> anyhow::Result<()> {
        self.install(source, local).await?;
        self.add_source_to_settings(source, local)?;
        Ok(())
    }

    /// Remove the installed copy of a source. Local paths are left alone.
    pub async fn remove(&self, source: &str, local: bool) -> anyhow::Result<()> {
        let parsed = parse_source(source);
        let scope = scope_for(local);
        self.assert_project_trusted(scope)?;
        let operation = async {
            match &parsed {
                ParsedSource::Npm(npm) => self.uninstall_npm(npm, scope).await,
                ParsedSource::Git(git) => self.remove_git(git, scope),
                ParsedSource::Archive(archive) => self.remove_archive(archive, scope),
                ParsedSource::Marketplace { name, .. } if self.marketplace_listed_elsewhere(name, scope) => Ok(()),
                ParsedSource::Marketplace { name, .. } => self.marketplace().remove(name),
                ParsedSource::Local(_) => Ok(()),
            }
        };
        self.with_progress(ProgressAction::Remove, source, format!("Removing {source}..."), operation).await
    }

    /// True when the settings of the scope other than `scope` list `marketplace:<name>` with
    /// any tag, so the shared install dir stays.
    pub(crate) fn marketplace_listed_elsewhere(&self, name: &str, scope: SourceScope) -> bool {
        let other = if scope == SourceScope::Project { SettingsScope::Global } else { SettingsScope::Project };
        self.settings.packages(other).iter().any(|package| {
            matches!(parse_source(package_source_text(package)), ParsedSource::Marketplace { name: listed, .. } if listed == name)
        })
    }

    /// `remove`, then drop the source from settings; false when settings had no match.
    pub async fn remove_and_persist(&self, source: &str, local: bool) -> anyhow::Result<bool> {
        self.remove(source, local).await?;
        self.remove_source_from_settings(source, local)
    }

    pub(crate) async fn install_parsed_source(&self, parsed: &ParsedSource, scope: SourceScope) -> anyhow::Result<()> {
        match parsed {
            ParsedSource::Npm(npm) => self.install_npm(npm, scope, scope == SourceScope::Temporary).await,
            ParsedSource::Git(git) => self.install_git(git, scope).await,
            ParsedSource::Archive(archive) => self.install_archive(archive, scope, false).await,
            ParsedSource::Marketplace { name, tag } => {
                self.marketplace().install(name, tag.as_deref()).await.map(|_| ())
            }
            ParsedSource::Local(_) => Ok(()),
        }
    }
}
