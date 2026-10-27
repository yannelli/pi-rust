//! npm package installs: the managed npm root, legacy global installs, `npm view`
//! version lookups, and update checks.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail};
use serde_json::Value;

use super::manifest::installed_version;
use super::paths::ensure_npm_project;
use super::runner::{NpmCommand, npm_install_args, npm_uninstall_args};
use super::source::{NpmSource, max_version, version_gt};
use super::{NETWORK_TIMEOUT, PackageManager, ProgressAction};
use crate::resources::SourceScope;

impl PackageManager {
    pub(crate) fn npm_command(&self) -> anyhow::Result<NpmCommand> {
        NpmCommand::from_settings(&self.settings)
    }

    pub(crate) fn package_manager_name(&self) -> anyhow::Result<String> {
        self.npm_command()?.package_manager_name()
    }

    pub(crate) async fn run_npm(&self, args: Vec<String>, cwd: Option<&Path>) -> anyhow::Result<()> {
        let mut spec = self.npm_command()?.spec(args);
        spec.cwd = cwd.map(Path::to_path_buf);
        self.runner.run(&spec).await
    }

    pub(crate) fn managed_npm_install_path(&self, source: &NpmSource, scope: SourceScope) -> anyhow::Result<PathBuf> {
        Ok(self.npm_install_root(scope, false)?.join("node_modules").join(&source.name))
    }

    /// The managed path, or for user packages a legacy global install when only that exists.
    pub(crate) async fn npm_install_path(&self, source: &NpmSource, scope: SourceScope) -> anyhow::Result<PathBuf> {
        let managed = self.managed_npm_install_path(source, scope)?;
        if scope != SourceScope::User || managed.exists() {
            return Ok(managed);
        }
        match self.legacy_global_npm_install_path(source).await {
            Some(legacy) if legacy.exists() => Ok(legacy),
            _ => Ok(managed),
        }
    }

    async fn legacy_global_npm_install_path(&self, source: &NpmSource) -> Option<PathBuf> {
        if let Ok(Some(path)) = self.pnpm_global_package_path(&source.name).await {
            return Some(path);
        }
        self.global_npm_root().await.ok().map(|root| root.join(&source.name))
    }

    /// `npm root -g` (or bun's global directory), cached per `npmCommand`.
    pub(crate) async fn global_npm_root(&self) -> anyhow::Result<PathBuf> {
        let command = self.npm_command()?;
        let key = command.key();
        let cached = self.global_npm_root.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
        if let Some((_, root)) = cached.filter(|(cached_key, _)| *cached_key == key) {
            return Ok(root);
        }
        let root = if command.package_manager_name()? == "bun" {
            let bin = self.runner.capture(&command.spec(["pm", "bin", "-g"])).await?;
            let parent = Path::new(bin.trim()).parent().map(Path::to_path_buf).unwrap_or_default();
            parent.join("install").join("global").join("node_modules")
        } else {
            PathBuf::from(self.runner.capture(&command.spec(["root", "-g"])).await?.trim())
        };
        *self.global_npm_root.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((key, root.clone()));
        Ok(root)
    }

    /// pnpm keeps global packages outside `npm root -g`; ask `pnpm list -g`.
    async fn pnpm_global_package_path(&self, name: &str) -> anyhow::Result<Option<PathBuf>> {
        let command = self.npm_command()?;
        if command.package_manager_name()? != "pnpm" {
            return Ok(None);
        }
        let output = self.runner.capture(&command.spec(["list", "-g", "--depth", "0", "--json"])).await?;
        let entries: Value = serde_json::from_str(&output)?;
        let found = entries
            .as_array()
            .into_iter()
            .flatten()
            .find_map(|entry| entry.get("dependencies")?.get(name)?.get("path")?.as_str().map(PathBuf::from));
        Ok(found)
    }

    pub(crate) async fn install_npm(
        &self,
        source: &NpmSource,
        scope: SourceScope,
        temporary: bool,
    ) -> anyhow::Result<()> {
        let root = self.npm_install_root(scope, temporary)?;
        ensure_npm_project(&root)?;
        let manager = self.package_manager_name()?;
        self.run_npm(npm_install_args(&manager, std::slice::from_ref(&source.spec), &root), None).await
    }

    pub(crate) async fn install_npm_batch(&self, specs: &[String], scope: SourceScope) -> anyhow::Result<()> {
        let root = self.npm_install_root(scope, false)?;
        ensure_npm_project(&root)?;
        let manager = self.package_manager_name()?;
        self.run_npm(npm_install_args(&manager, specs, &root), None).await
    }

    pub(crate) async fn uninstall_npm(&self, source: &NpmSource, scope: SourceScope) -> anyhow::Result<()> {
        let root = self.npm_install_root(scope, false)?;
        if !root.exists() {
            return Ok(());
        }
        let manager = self.package_manager_name()?;
        self.run_npm(npm_uninstall_args(&manager, &source.name, &root), None).await
    }

    /// The newest published version (within `range` when given) from `npm view`.
    pub(crate) async fn latest_npm_version(&self, spec: &str, range: Option<&str>) -> anyhow::Result<String> {
        let command = self.npm_command()?;
        let lookup = command.spec(["view", spec, "version", "--json"]).in_dir(&self.cwd).with_timeout(NETWORK_TIMEOUT);
        let stdout = self.runner.capture(&lookup).await?;
        let raw = stdout.trim();
        if raw.is_empty() {
            bail!("Empty response from npm view");
        }
        match serde_json::from_str::<Value>(raw)? {
            Value::String(version) => Ok(version),
            Value::Array(values) => {
                let versions: Vec<String> =
                    values.iter().filter_map(Value::as_str).filter(|v| !v.is_empty()).map(String::from).collect();
                max_version(&versions, range).ok_or_else(|| anyhow!("Unexpected response from npm view"))
            }
            _ => bail!("Unexpected response from npm view"),
        }
    }

    fn lookup_target(source: &NpmSource) -> (&str, Option<&str>) {
        let spec = if source.version.is_some() { source.spec.as_str() } else { source.name.as_str() };
        (spec, source.version.as_deref().filter(|_| source.has_range))
    }

    /// Update when not installed, when the lookup fails, or when a newer version exists.
    pub(crate) async fn should_update_npm_source(&self, source: &NpmSource, scope: SourceScope) -> bool {
        let Ok(path) = self.managed_npm_install_path(source, scope) else { return true };
        let Some(installed) = path.exists().then(|| installed_version(&path)).flatten() else { return true };
        let (spec, range) = Self::lookup_target(source);
        match self.latest_npm_version(spec, range).await {
            Ok(target) => version_gt(&target, &installed).unwrap_or(true),
            Err(_) => true,
        }
    }

    /// Refresh an unpinned `-e` package when a newer version exists, as git sources refresh; a
    /// failed refresh keeps the cached install (upstream issue 11375).
    pub(crate) async fn refresh_temporary_npm_source(&self, source: &NpmSource, source_text: &str, installed: &Path) {
        if source.pinned || !self.npm_has_available_update(source, installed).await {
            return;
        }
        let spec = if source.version.is_some() { source.spec.clone() } else { format!("{}@latest", source.name) };
        let refresh = async {
            let root = self.npm_install_root(SourceScope::Temporary, true)?;
            ensure_npm_project(&root)?;
            let manager = self.package_manager_name()?;
            self.run_npm(npm_install_args(&manager, &[spec], &root), None).await
        };
        let message = format!("Updating {source_text}...");
        let _ = self.with_progress(ProgressAction::Update, source_text, message, refresh).await;
    }

    pub(crate) async fn npm_has_available_update(&self, source: &NpmSource, installed_path: &Path) -> bool {
        if self.is_offline() {
            return false;
        }
        let Some(installed) = installed_version(installed_path) else { return false };
        let (spec, range) = Self::lookup_target(source);
        match self.latest_npm_version(spec, range).await {
            Ok(target) => version_gt(&target, &installed).unwrap_or(false),
            Err(_) => false,
        }
    }
}

/// Whether the package at `installed_path` satisfies the configured version.
pub(crate) fn installed_npm_matches(source: &NpmSource, installed_path: &Path) -> bool {
    installed_version(installed_path).is_some_and(|version| source.accepts(&version))
}
