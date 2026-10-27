//! Resolving configured packages and `-e` sources into resource paths for the resource
//! loader: manifests, conventional directories, settings filters, and `autoload` deltas.

use std::path::{Path, PathBuf};

use anyhow::bail;

use super::glob::expand_package_glob;
use super::manifest::read_pi_manifest;
use super::npm::installed_npm_matches;
use super::settings_ops::package_source_text;
use super::source::{LocalSource, ParsedSource, parse_source};
use super::{MissingSourceAction, MissingSourceHandler, PackageInstallFailure, PackageManager};
use crate::resources::discovery::{
    LocalResourceOptions, ResourceAccumulator, collect_resource_files, resolve_local_resources,
};
use crate::resources::filters::{apply_patterns, autoload_override};
use crate::resources::paths::{file_kind, normalize};
use crate::resources::source_info::BUILTIN_PATH_PREFIX;
use crate::resources::{PathMetadata, ResolvedPaths, ResourceSettings, ResourceType, SourceOrigin, SourceScope};
use crate::settings::schema::{PackageFilter, PackageSource, Settings};

pub(crate) type ScopedPackage = (PackageSource, SourceScope);

fn filter_entries(filter: &PackageFilter, kind: ResourceType) -> Option<&Vec<String>> {
    match kind {
        ResourceType::Extensions => filter.extensions.as_ref(),
        ResourceType::Skills => filter.skills.as_ref(),
        ResourceType::Prompts => filter.prompts.as_ref(),
        ResourceType::Themes => filter.themes.as_ref(),
    }
}

fn is_override(entry: &str) -> bool {
    entry.starts_with(['!', '+', '-'])
}

fn collect_files_from_paths(paths: &[PathBuf], kind: ResourceType) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for path in paths {
        match file_kind(path) {
            Some((true, _)) => files.push(path.clone()),
            Some((_, true)) => files.extend(collect_resource_files(path, kind)),
            _ => {}
        }
    }
    files
}

/// Files named by manifest entries: exact paths and expanded globs; overrides are skipped.
fn collect_files_from_manifest_entries(entries: &[String], root: &Path, kind: ResourceType) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for entry in entries.iter().filter(|entry| !is_override(entry)) {
        if entry.contains(['*', '?']) {
            paths.extend(expand_package_glob(entry, root));
        } else {
            paths.push(normalize(&root.join(entry)));
        }
    }
    collect_files_from_paths(&paths, kind)
}

/// Files a package offers for `kind` after its own manifest overrides.
fn collect_manifest_files(root: &Path, kind: ResourceType) -> Vec<PathBuf> {
    let manifest = read_pi_manifest(&root.join("package.json"));
    if let Some(entries) = manifest.as_ref().and_then(|manifest| manifest.entries(kind)).filter(|e| !e.is_empty()) {
        let files = collect_files_from_manifest_entries(entries, root, kind);
        let overrides: Vec<String> = entries.iter().filter(|entry| is_override(entry)).cloned().collect();
        if overrides.is_empty() {
            return files;
        }
        let enabled = apply_patterns(&files, &overrides, root);
        return files.into_iter().filter(|file| enabled.contains(file)).collect();
    }
    let dir = root.join(kind.dir_name());
    if dir.exists() { collect_resource_files(&dir, kind) } else { Vec::new() }
}

fn add_manifest_entries(
    acc: &mut ResourceAccumulator,
    entries: &[String],
    root: &Path,
    kind: ResourceType,
    metadata: &PathMetadata,
) {
    let files = collect_files_from_manifest_entries(entries, root, kind);
    let overrides: Vec<String> = entries.iter().filter(|entry| is_override(entry)).cloned().collect();
    let enabled = apply_patterns(&files, &overrides, root);
    for file in files.into_iter().filter(|file| enabled.contains(file)) {
        acc.add(kind, file, metadata.clone(), true);
    }
}

fn collect_default_resources(acc: &mut ResourceAccumulator, root: &Path, kind: ResourceType, metadata: &PathMetadata) {
    let manifest = read_pi_manifest(&root.join("package.json"));
    if let Some(entries) = manifest.as_ref().and_then(|manifest| manifest.entries(kind)) {
        add_manifest_entries(acc, entries, root, kind, metadata);
        return;
    }
    let dir = root.join(kind.dir_name());
    for file in if dir.exists() { collect_resource_files(&dir, kind) } else { Vec::new() } {
        acc.add(kind, file, metadata.clone(), true);
    }
}

/// Add a package's resources. Returns false when the package has no manifest, no filter,
/// and no conventional directory (so a local directory loads as one extension).
pub(crate) fn collect_package_resources(
    acc: &mut ResourceAccumulator,
    root: &Path,
    filter: Option<&PackageFilter>,
    metadata: &PathMetadata,
) -> bool {
    if let Some(filter) = filter {
        for kind in ResourceType::ALL {
            let patterns = filter_entries(filter, kind);
            if filter.autoload == Some(false) {
                let files = match patterns {
                    Some(patterns) if !patterns.is_empty() => collect_manifest_files(root, kind),
                    _ => continue,
                };
                let patterns = patterns.cloned().unwrap_or_default();
                for file in files {
                    let decision = autoload_override(&file, &patterns, root);
                    if let Some(enabled) = decision {
                        acc.add(kind, file, metadata.clone(), enabled);
                    }
                }
            } else if let Some(patterns) = patterns {
                let files = collect_manifest_files(root, kind);
                let enabled = apply_patterns(&files, patterns, root);
                for file in files {
                    let is_enabled = !patterns.is_empty() && enabled.contains(&file);
                    acc.add(kind, file, metadata.clone(), is_enabled);
                }
            } else {
                collect_default_resources(acc, root, kind, metadata);
            }
        }
        return true;
    }
    if let Some(manifest) = read_pi_manifest(&root.join("package.json")) {
        for kind in ResourceType::ALL {
            if let Some(entries) = manifest.entries(kind) {
                add_manifest_entries(acc, entries, root, kind, metadata);
            }
        }
        return true;
    }
    let mut found = false;
    for kind in ResourceType::ALL {
        let dir = root.join(kind.dir_name());
        if dir.exists() {
            found = true;
            for file in collect_resource_files(&dir, kind) {
                acc.add(kind, file, metadata.clone(), true);
            }
        }
    }
    found
}

fn resource_settings(settings: &Settings) -> ResourceSettings {
    ResourceSettings {
        extensions: settings.extensions.clone().unwrap_or_default(),
        skills: settings.skills.clone().unwrap_or_default(),
        prompts: settings.prompts.clone().unwrap_or_default(),
        themes: settings.themes.clone().unwrap_or_default(),
    }
}

impl PackageManager {
    /// Everything upstream `resolve` returns: package resources, settings entries,
    /// auto-discovered directories, and built-in extensions.
    pub async fn resolve(&self, on_missing: Option<&MissingSourceHandler>) -> anyhow::Result<ResolvedPaths> {
        let packages = self.resolve_package_resources(on_missing).await?;
        let user = resource_settings(&self.settings.global_settings());
        let project = resource_settings(&self.settings.project_settings());
        let options = LocalResourceOptions {
            cwd: &self.cwd,
            agent_dir: &self.agent_dir,
            home: &self.home,
            project_trusted: self.settings.is_project_trusted(),
            user: &user,
            project: &project,
            builtin_extensions: &self.builtin_extensions,
        };
        Ok(resolve_local_resources(&options, &packages))
    }

    /// Resources of the `packages` settings only: the `package_resources` input of
    /// [`crate::resources::ResourceLoaderOptions`]. Missing packages install first.
    pub async fn resolve_package_resources(
        &self,
        on_missing: Option<&MissingSourceHandler>,
    ) -> anyhow::Result<ResolvedPaths> {
        let mut all: Vec<ScopedPackage> = Vec::new();
        all.extend(
            self.settings
                .packages(crate::settings::SettingsScope::Project)
                .into_iter()
                .map(|p| (p, SourceScope::Project)),
        );
        all.extend(
            self.settings.packages(crate::settings::SettingsScope::Global).into_iter().map(|p| (p, SourceScope::User)),
        );
        let sources = self.dedupe_packages(all)?;
        let mut acc = ResourceAccumulator::default();
        self.resolve_package_sources(&sources, &mut acc, on_missing).await?;
        Ok(acc.into_resolved())
    }

    /// Resources of `-e` sources: the `extension_source_resources` input of the resource
    /// loader (pass `temporary` for `-e`, so packages install under the temp folder).
    pub async fn resolve_extension_sources(
        &self,
        sources: &[String],
        local: bool,
        temporary: bool,
    ) -> anyhow::Result<ResolvedPaths> {
        let scope = if temporary {
            SourceScope::Temporary
        } else if local {
            SourceScope::Project
        } else {
            SourceScope::User
        };
        let mut acc = ResourceAccumulator::default();
        for source in sources.iter().filter(|source| source.starts_with(BUILTIN_PATH_PREFIX)) {
            let metadata = PathMetadata::new("builtin", scope, SourceOrigin::TopLevel);
            acc.add(ResourceType::Extensions, PathBuf::from(source), metadata, true);
        }
        let packages: Vec<ScopedPackage> = sources
            .iter()
            .filter(|source| !source.starts_with(BUILTIN_PATH_PREFIX))
            .map(|source| (PackageSource::Source(source.clone()), scope))
            .collect();
        self.resolve_package_sources(&packages, &mut acc, None).await?;
        Ok(acc.into_resolved())
    }

    /// Project entries replace user entries with the same identity, except that a project
    /// entry with `autoload: false` is a delta over the user entry, so both stay.
    pub(crate) fn dedupe_packages(&self, packages: Vec<ScopedPackage>) -> anyhow::Result<Vec<ScopedPackage>> {
        let mut result: Vec<ScopedPackage> = Vec::new();
        let mut seen: Vec<(String, usize)> = Vec::new();
        for entry in packages {
            let identity = self.package_identity(package_source_text(&entry.0), Some(entry.1))?;
            let Some(&(_, index)) = seen.iter().find(|(known, _)| *known == identity) else {
                seen.push((identity, result.len()));
                result.push(entry);
                continue;
            };
            let existing = &result[index];
            if existing.1 == SourceScope::Project && entry.1 == SourceScope::User {
                if matches!(&existing.0, PackageSource::Filtered(filter) if filter.autoload == Some(false)) {
                    result.push(entry);
                }
            } else if entry.1 == SourceScope::Project {
                result[index] = entry;
            }
        }
        Ok(result)
    }

    fn autoload_delta_base(
        &self,
        entry: &ScopedPackage,
        sources: &[ScopedPackage],
    ) -> anyhow::Result<Option<(String, SourceScope)>> {
        let PackageSource::Filtered(filter) = &entry.0 else { return Ok(None) };
        if entry.1 != SourceScope::Project || filter.autoload != Some(false) {
            return Ok(None);
        }
        let identity = self.package_identity(&filter.source, Some(entry.1))?;
        for (package, scope) in sources.iter().filter(|(_, scope)| *scope == SourceScope::User) {
            let text = package_source_text(package);
            if self.package_identity(text, Some(*scope))? == identity {
                return Ok(Some((text.to_string(), SourceScope::User)));
            }
        }
        Ok(None)
    }

    async fn install_missing(
        &self,
        parsed: &ParsedSource,
        source: &str,
        scope: SourceScope,
        on_missing: Option<&MissingSourceHandler>,
    ) -> anyhow::Result<bool> {
        if self.is_offline() {
            return Ok(false);
        }
        if let Some(handler) = on_missing {
            match handler(source.to_string()).await {
                MissingSourceAction::Skip => return Ok(false),
                MissingSourceAction::Error => bail!("Missing source: {source}"),
                MissingSourceAction::Install => {}
            }
        }
        // A package that fails to install is skipped like an offline one, so the others still load.
        if let Err(error) = self.install_parsed_source(parsed, scope).await {
            let failure = PackageInstallFailure { source: source.to_string(), scope, message: format!("{error:#}") };
            self.install_failures.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(failure);
            return Ok(false);
        }
        Ok(true)
    }

    async fn resolve_package_sources(
        &self,
        sources: &[ScopedPackage],
        acc: &mut ResourceAccumulator,
        on_missing: Option<&MissingSourceHandler>,
    ) -> anyhow::Result<()> {
        for entry in sources {
            let (package, scope) = (&entry.0, entry.1);
            let source = package_source_text(package);
            let filter = match package {
                PackageSource::Filtered(filter) => Some(filter),
                PackageSource::Source(_) => None,
            };
            let (resolved_source, resolved_scope) =
                self.autoload_delta_base(entry, sources)?.unwrap_or_else(|| (source.to_string(), scope));
            let parsed = parse_source(&resolved_source);
            let mut metadata = PathMetadata::new(source, scope, SourceOrigin::Package);
            let installed = match &parsed {
                ParsedSource::Local(local) => {
                    let base = self.base_dir_for_scope(resolved_scope)?;
                    resolve_local_package(acc, local, filter, metadata, &base);
                    continue;
                }
                ParsedSource::Npm(npm) => {
                    let path = self.npm_install_path(npm, resolved_scope).await?;
                    if path.exists() && installed_npm_matches(npm, &path) {
                        if resolved_scope == SourceScope::Temporary {
                            self.refresh_temporary_npm_source(npm, &resolved_source, &path).await;
                        }
                        path
                    } else if self.install_missing(&parsed, &resolved_source, resolved_scope, on_missing).await? {
                        self.npm_install_path(npm, resolved_scope).await?
                    } else {
                        continue;
                    }
                }
                ParsedSource::Git(git) => {
                    let path = self.git_install_path(git, resolved_scope)?;
                    if !path.exists() {
                        if !self.install_missing(&parsed, &resolved_source, resolved_scope, on_missing).await? {
                            continue;
                        }
                    } else if resolved_scope == SourceScope::Temporary && !git.pinned {
                        self.refresh_temporary_git_source(git, &resolved_source).await;
                    }
                    path
                }
                ParsedSource::Archive(archive) => {
                    let path = self.archive_install_path(archive, resolved_scope)?;
                    if !path.exists()
                        && !self.install_missing(&parsed, &resolved_source, resolved_scope, on_missing).await?
                    {
                        continue;
                    }
                    path
                }
            };
            metadata.base_dir = Some(installed.clone());
            metadata.package_root = Some(installed.clone());
            collect_package_resources(acc, &installed, filter, &metadata);
        }
        Ok(())
    }
}

/// A local file loads as one extension; a directory follows package discovery, and a
/// directory without resources loads as one extension.
fn resolve_local_package(
    acc: &mut ResourceAccumulator,
    source: &LocalSource,
    filter: Option<&PackageFilter>,
    mut metadata: PathMetadata,
    base: &Path,
) {
    let resolved = crate::resources::paths::resolve_path(&source.path, base);
    match file_kind(&resolved) {
        Some((true, _)) => {
            metadata.base_dir = resolved.parent().map(Path::to_path_buf);
            acc.add(ResourceType::Extensions, resolved, metadata, true);
        }
        Some((_, true)) => {
            metadata.base_dir = Some(resolved.clone());
            metadata.package_root = Some(resolved.clone());
            if !collect_package_resources(acc, &resolved, filter, &metadata) {
                acc.add(ResourceType::Extensions, resolved, metadata, true);
            }
        }
        _ => {}
    }
}
