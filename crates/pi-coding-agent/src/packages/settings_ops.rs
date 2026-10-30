//! The `packages` settings list: adding and removing sources, package identity, installed
//! paths, and `pi list` data.

use std::path::{Component, Path, PathBuf};

use super::source::{ParsedSource, parse_source};
use super::{ConfiguredPackage, PackageManager, settings_scope};
use crate::resources::SourceScope;
use crate::resources::paths::{relative_posix, resolve_path};
use crate::settings::schema::PackageSource;

pub(crate) fn package_source_text(package: &PackageSource) -> &str {
    match package {
        PackageSource::Source(source) => source,
        PackageSource::Filtered(filter) => &filter.source,
    }
}

/// `target` relative to `base` (`.` when equal), or absolute across Windows drives.
pub(crate) fn relative_or_absolute(base: &Path, target: &Path) -> String {
    let first = |path: &Path| path.components().next().map(|component| component.as_os_str().to_os_string());
    let same_root = first(base) == first(target) || !matches!(base.components().next(), Some(Component::Prefix(_)));
    if !same_root {
        return target.display().to_string();
    }
    let relative = relative_posix(base, target);
    if relative.is_empty() { ".".into() } else { relative }
}

impl PackageManager {
    fn parsed_identity(&self, parsed: &ParsedSource, local_base: Option<&Path>) -> String {
        match parsed {
            ParsedSource::Npm(npm) => format!("npm:{}", npm.name),
            ParsedSource::Git(git) => format!("git:{}/{}", git.host, git.path),
            ParsedSource::Archive(archive) => format!("url:{}/{}", archive.host, archive.path),
            ParsedSource::Marketplace { name, .. } => format!("marketplace:{name}"),
            ParsedSource::Local(local) => {
                let base = local_base.unwrap_or(&self.cwd);
                format!("local:{}", resolve_path(&local.path, base).display())
            }
        }
    }

    /// Identity ignoring version and ref: npm name, git host and path, archive URL path, or the
    /// absolute local path (resolved from the scope's settings directory when given).
    pub fn package_identity(&self, source: &str, scope: Option<SourceScope>) -> anyhow::Result<String> {
        let parsed = parse_source(source);
        let base = match (&parsed, scope) {
            (ParsedSource::Local(_), Some(scope)) => Some(self.base_dir_for_scope(scope)?),
            _ => None,
        };
        Ok(self.parsed_identity(&parsed, base.as_deref()))
    }

    fn package_sources_match(&self, existing: &PackageSource, input: &str, scope: SourceScope) -> anyhow::Result<bool> {
        let left = self.package_identity(package_source_text(existing), Some(scope))?;
        Ok(left == self.package_identity(input, None)?)
    }

    /// The form stored in settings: local paths become relative to the settings directory.
    fn normalize_source_for_settings(&self, source: &str, scope: SourceScope) -> anyhow::Result<String> {
        let ParsedSource::Local(local) = parse_source(source) else { return Ok(source.to_string()) };
        let base = self.base_dir_for_scope(scope)?;
        Ok(relative_or_absolute(&base, &self.resolve_input_path(&local.path)))
    }

    fn configured(&self, scope: SourceScope) -> Vec<PackageSource> {
        self.settings.packages(settings_scope(scope))
    }

    fn store(&self, scope: SourceScope, packages: &[PackageSource]) -> anyhow::Result<()> {
        if scope == SourceScope::Project {
            self.settings.set_project_packages(packages).map_err(anyhow::Error::msg)
        } else {
            self.settings.set_packages(packages);
            Ok(())
        }
    }

    /// Add a source to user (or with `local`, project) settings. A source with the same
    /// identity is replaced in place, keeping its filters; false when nothing changed.
    pub fn add_source_to_settings(&self, source: &str, local: bool) -> anyhow::Result<bool> {
        let scope = if local { SourceScope::Project } else { SourceScope::User };
        let mut packages = self.configured(scope);
        let normalized = self.normalize_source_for_settings(source, scope)?;
        let mut matched = None;
        for (index, existing) in packages.iter().enumerate() {
            if self.package_sources_match(existing, source, scope)? {
                matched = Some(index);
                break;
            }
        }
        match matched {
            Some(index) if package_source_text(&packages[index]) == normalized => return Ok(false),
            Some(index) => match &mut packages[index] {
                PackageSource::Source(text) => *text = normalized,
                PackageSource::Filtered(filter) => filter.source = normalized,
            },
            None => packages.push(PackageSource::Source(normalized)),
        }
        self.store(scope, &packages)?;
        Ok(true)
    }

    /// Remove every entry with the source's identity; false when none matched.
    pub fn remove_source_from_settings(&self, source: &str, local: bool) -> anyhow::Result<bool> {
        let scope = if local { SourceScope::Project } else { SourceScope::User };
        let packages = self.configured(scope);
        let mut kept = Vec::with_capacity(packages.len());
        for package in &packages {
            if !self.package_sources_match(package, source, scope)? {
                kept.push(package.clone());
            }
        }
        if kept.len() == packages.len() {
            return Ok(false);
        }
        self.store(scope, &kept)?;
        Ok(true)
    }

    /// Where an installed source lives, when it exists.
    pub async fn installed_path(&self, source: &str, scope: SourceScope) -> Option<PathBuf> {
        let path = match parse_source(source) {
            ParsedSource::Npm(npm) => self.npm_install_path(&npm, scope).await.ok()?,
            ParsedSource::Git(git) => self.git_install_path(&git, scope).ok()?,
            ParsedSource::Archive(archive) => self.archive_install_path(&archive, scope).ok()?,
            ParsedSource::Marketplace { name, .. } => self.marketplace().installed_package(&name).ok()??.path,
            ParsedSource::Local(local) => resolve_path(&local.path, &self.base_dir_for_scope(scope).ok()?),
        };
        path.exists().then_some(path)
    }

    /// User packages, then project packages, as `pi list` shows them.
    pub async fn list_configured_packages(&self) -> Vec<ConfiguredPackage> {
        let mut out = Vec::new();
        for scope in [SourceScope::User, SourceScope::Project] {
            for package in self.configured(scope) {
                let source = package_source_text(&package).to_string();
                let installed_path = self.installed_path(&source, scope).await;
                let filtered = matches!(package, PackageSource::Filtered(_));
                out.push(ConfiguredPackage { source, scope, filtered, installed_path });
            }
        }
        out
    }

    /// `No matching package found for <source>`, with a suggestion when the input names a
    /// configured npm package without `npm:` or a git repository without `git:`.
    pub(crate) fn no_matching_package_message(&self, source: &str, configured: &[PackageSource]) -> String {
        let trimmed = source.trim();
        let suggestion = configured.iter().map(package_source_text).find(|text| match parse_source(text) {
            ParsedSource::Npm(npm) => trimmed == npm.name || trimmed == npm.spec,
            ParsedSource::Marketplace { name, .. } => trimmed == name,
            ParsedSource::Git(git) => {
                let shorthand = format!("{}/{}", git.host, git.path);
                trimmed == shorthand || git.git_ref.is_some_and(|git_ref| trimmed == format!("{shorthand}@{git_ref}"))
            }
            _ => false,
        });
        match suggestion {
            Some(suggestion) => format!("No matching package found for {source}. Did you mean {suggestion}?"),
            None => format!("No matching package found for {source}"),
        }
    }
}
