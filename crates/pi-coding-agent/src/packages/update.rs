//! `pi update` for packages and background update checks. Unpinned npm packages update in
//! one install per scope, git checkouts reconcile in parallel, and URL archives download
//! again only when the server reports a change.

use futures::FutureExt;
use futures::future::BoxFuture;

use super::settings_ops::package_source_text;
use super::source::{NpmSource, ParsedSource, parse_source};
use super::{
    GIT_UPDATE_CONCURRENCY, PackageKind, PackageManager, PackageUpdate, ProgressAction, UPDATE_CHECK_CONCURRENCY,
    run_with_concurrency,
};
use crate::resources::SourceScope;
use crate::settings::SettingsScope;

impl PackageManager {
    /// Update every configured package, or those with the identity of `source`.
    pub async fn update(&self, source: Option<&str>) -> anyhow::Result<()> {
        let user = self.settings.packages(SettingsScope::Global);
        let project = self.settings.packages(SettingsScope::Project);
        let identity = source.map(|source| self.package_identity(source, None)).transpose()?;
        let mut targets = Vec::new();
        for (packages, scope) in [(&user, SourceScope::User), (&project, SourceScope::Project)] {
            for package in packages {
                let text = package_source_text(package);
                if let Some(identity) = &identity
                    && self.package_identity(text, Some(scope))? != *identity
                {
                    continue;
                }
                targets.push((text.to_string(), scope));
            }
        }
        if let Some(source) = source
            && targets.is_empty()
        {
            let configured: Vec<_> = user.iter().chain(&project).cloned().collect();
            anyhow::bail!(self.no_matching_package_message(source, &configured));
        }
        self.update_configured_sources(targets).await
    }

    async fn update_configured_sources(&self, sources: Vec<(String, SourceScope)>) -> anyhow::Result<()> {
        if self.is_offline() || sources.is_empty() {
            return Ok(());
        }
        let mut npm = Vec::new();
        let mut others = Vec::new();
        for (source, scope) in sources {
            match parse_source(&source) {
                ParsedSource::Npm(parsed) if !parsed.pinned => npm.push((source, scope, parsed)),
                parsed @ ParsedSource::Marketplace { tag: None, .. } => others.push((source, scope, parsed)),
                parsed @ (ParsedSource::Git(_) | ParsedSource::Archive(_)) => others.push((source, scope, parsed)),
                _ => {}
            }
        }
        let checks: Vec<BoxFuture<'_, bool>> =
            npm.iter().map(|(_, scope, parsed)| self.should_update_npm_source(parsed, *scope).boxed()).collect();
        let decisions = run_with_concurrency(checks, UPDATE_CHECK_CONCURRENCY).await;
        let selected: Vec<&(String, SourceScope, NpmSource)> =
            npm.iter().zip(decisions).filter(|(_, update)| *update).map(|(entry, _)| entry).collect();
        let mut tasks: Vec<BoxFuture<'_, anyhow::Result<()>>> = Vec::new();
        for scope in [SourceScope::User, SourceScope::Project] {
            let batch: Vec<&(String, SourceScope, NpmSource)> =
                selected.iter().copied().filter(|(_, entry_scope, _)| *entry_scope == scope).collect();
            if !batch.is_empty() {
                tasks.push(self.update_npm_batch(batch, scope).boxed());
            }
        }
        if !others.is_empty() {
            tasks.push(self.update_checkouts(&others).boxed());
        }
        let results = futures::future::join_all(tasks).await;
        results.into_iter().collect()
    }

    async fn update_checkouts(&self, entries: &[(String, SourceScope, ParsedSource)]) -> anyhow::Result<()> {
        let tasks: Vec<BoxFuture<'_, anyhow::Result<()>>> = entries
            .iter()
            .map(|(source, scope, parsed)| {
                let operation = async move {
                    match parsed {
                        ParsedSource::Git(git) => self.update_git(git, *scope).await,
                        ParsedSource::Archive(archive) => self.install_archive(archive, *scope, true).await,
                        ParsedSource::Marketplace { name, .. } => self.marketplace().update(name).await.map(|_| ()),
                        _ => Ok(()),
                    }
                };
                self.with_progress(ProgressAction::Update, source, format!("Updating {source}..."), operation).boxed()
            })
            .collect();
        run_with_concurrency(tasks, GIT_UPDATE_CONCURRENCY).await.into_iter().collect()
    }

    async fn update_npm_batch(
        &self,
        entries: Vec<&(String, SourceScope, NpmSource)>,
        scope: SourceScope,
    ) -> anyhow::Result<()> {
        let scope_name = if scope == SourceScope::Project { "project" } else { "user" };
        let (label, message) = match entries.as_slice() {
            [(source, _, _)] => (source.clone(), format!("Updating {source}...")),
            _ => (format!("{scope_name} npm packages"), format!("Updating {scope_name} npm packages...")),
        };
        let specs: Vec<String> = entries
            .iter()
            .map(
                |(_, _, parsed)| {
                    if parsed.version.is_some() { parsed.spec.clone() } else { format!("{}@latest", parsed.name) }
                },
            )
            .collect();
        self.with_progress(ProgressAction::Update, &label, message, self.install_npm_batch(&specs, scope)).await
    }

    /// Installed, unpinned packages with a newer version (project entries win duplicates).
    pub async fn check_for_available_updates(&self) -> Vec<PackageUpdate> {
        if self.is_offline() {
            return Vec::new();
        }
        let mut all = Vec::new();
        all.extend(self.settings.packages(SettingsScope::Project).into_iter().map(|p| (p, SourceScope::Project)));
        all.extend(self.settings.packages(SettingsScope::Global).into_iter().map(|p| (p, SourceScope::User)));
        let Ok(packages) = self.dedupe_packages(all) else { return Vec::new() };
        let checks: Vec<BoxFuture<'_, Option<PackageUpdate>>> = packages
            .iter()
            .filter(|(_, scope)| *scope != SourceScope::Temporary)
            .map(|(package, scope)| self.check_package_update(package_source_text(package).to_string(), *scope).boxed())
            .collect();
        run_with_concurrency(checks, UPDATE_CHECK_CONCURRENCY).await.into_iter().flatten().collect()
    }

    async fn check_package_update(&self, source: String, scope: SourceScope) -> Option<PackageUpdate> {
        let parsed = parse_source(&source);
        if parsed.is_pinned() {
            return None;
        }
        let (display_name, kind) = match &parsed {
            ParsedSource::Npm(npm) => {
                let path = self.npm_install_path(npm, scope).await.ok().filter(|path| path.exists())?;
                self.npm_has_available_update(npm, &path).await.then(|| (npm.name.clone(), PackageKind::Npm))?
            }
            ParsedSource::Git(git) => {
                let path = self.git_install_path(git, scope).ok().filter(|path| path.exists())?;
                let name = format!("{}/{}", git.host, git.path);
                self.git_has_available_update(&path).await.then_some((name, PackageKind::Git))?
            }
            ParsedSource::Archive(archive) => {
                let path = self.archive_install_path(archive, scope).ok().filter(|path| path.exists())?;
                let name = format!("{}/{}", archive.host, archive.path);
                self.archive_has_available_update(archive, &path).await.then_some((name, PackageKind::Url))?
            }
            ParsedSource::Marketplace { name, .. } => {
                self.marketplace().has_update(name).await.ok()?.then(|| (name.clone(), PackageKind::Marketplace))?
            }
            ParsedSource::Local(_) => return None,
        };
        Some(PackageUpdate { source, display_name, kind, scope })
    }
}
