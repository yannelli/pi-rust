//! Pi packages: install, remove, update, list, and resolve npm, git, URL archive, and
//! local package sources. Ported from upstream `core/package-manager.ts`,
//! `core/pi-manifest.ts`, and `package-manager-cli.ts` (see `docs/packages.md`).

mod archive;
mod cli;
mod cli_parse;
mod cli_support;
pub mod config;
mod config_cmd;
mod config_editor;
mod git;
pub mod git_url;
mod glob;
mod install;
pub mod manifest;
mod npm;
mod paths;
mod resolve;
pub mod runner;
mod settings_ops;
pub mod source;
mod update;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use serde::Serialize;

use crate::resources::SourceScope;
use crate::settings::{SettingsManager, SettingsScope};

pub use cli::{PackageCommandOptions, run_package_command};
pub use cli_support::{
    CommandOutput, ModelsUpdateFn, SelfUpdateFn, SelfUpdateRequest, TrustExtensionsFn, TrustPromptFn,
    prompt_trust_on_terminal,
};
pub use config::{
    ConfigGroup, ConfigItem, ConfigSubgroup, ConfigWriteScope, ProjectOverrideState, ScopedResolvedPaths,
    build_config_groups,
};
pub use config_cmd::{ConfigSelectorFn, ConfigSelectorRequest, render_config_text, run_config_command};
pub use config_editor::ConfigEditor;
pub use git_url::{GitSource, parse_git_url};
pub use manifest::{PiManifest, collect_extension_package_warnings, read_pi_manifest};
pub use paths::extension_temp_folder;
pub use runner::{CommandRunner, CommandSpec, ProcessRunner};
pub use source::{ArchiveSource, LocalSource, NpmSource, ParsedSource, parse_source};

/// Timeout of `npm view`, `git ls-remote`, and other lookups that reach the network.
pub const NETWORK_TIMEOUT: Duration = Duration::from_secs(10);
const UPDATE_CHECK_CONCURRENCY: usize = 4;
const GIT_UPDATE_CONCURRENCY: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProgressKind {
    Start,
    Progress,
    Complete,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProgressAction {
    Install,
    Remove,
    Update,
    Clone,
    Pull,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProgressEvent {
    #[serde(rename = "type")]
    pub kind: ProgressKind,
    pub action: ProgressAction,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

pub type ProgressCallback = Arc<dyn Fn(&ProgressEvent) + Send + Sync>;

/// What to do with a configured package that is not installed yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MissingSourceAction {
    Install,
    Skip,
    Error,
}

pub type MissingSourceHandler = Arc<dyn Fn(String) -> BoxFuture<'static, MissingSourceAction> + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PackageKind {
    Npm,
    Git,
    Url,
}

/// An installed, unpinned package with a newer version available.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageUpdate {
    pub source: String,
    pub display_name: String,
    #[serde(rename = "type")]
    pub kind: PackageKind,
    pub scope: SourceScope,
}

/// A `packages` entry of user or project settings, for `pi list`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfiguredPackage {
    pub source: String,
    pub scope: SourceScope,
    pub filtered: bool,
    pub installed_path: Option<PathBuf>,
}

pub struct PackageManagerOptions {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    /// Home directory for `~/.agents/skills` discovery in [`PackageManager::resolve`].
    pub home: PathBuf,
    pub settings: Arc<SettingsManager>,
    /// Runs `npm`, `git`, and the `npmCommand` setting; tests pass a mock.
    pub runner: Arc<dyn CommandRunner>,
    /// Names of built-in extensions, resolved as `builtin:<name>` extension resources.
    pub builtin_extensions: Vec<String>,
    /// Client for URL archive downloads; defaults to the shared pi client.
    pub http_client: Option<reqwest::Client>,
    /// Overrides `PI_OFFLINE`.
    pub offline: Option<bool>,
}

impl PackageManagerOptions {
    pub fn new(cwd: &Path, agent_dir: &Path, settings: Arc<SettingsManager>) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            agent_dir: agent_dir.to_path_buf(),
            home: crate::resources::paths::home_dir(),
            settings,
            runner: Arc::new(ProcessRunner::default()),
            builtin_extensions: Vec::new(),
            http_client: None,
            offline: None,
        }
    }
}

/// Upstream `DefaultPackageManager`.
pub struct PackageManager {
    pub(crate) cwd: PathBuf,
    pub(crate) agent_dir: PathBuf,
    pub(crate) home: PathBuf,
    pub(crate) settings: Arc<SettingsManager>,
    pub(crate) runner: Arc<dyn CommandRunner>,
    pub(crate) builtin_extensions: Vec<String>,
    http_client: Option<reqwest::Client>,
    offline: Option<bool>,
    progress: Mutex<Option<ProgressCallback>>,
    global_npm_root: Mutex<Option<(String, PathBuf)>>,
}

impl PackageManager {
    pub fn new(options: PackageManagerOptions) -> Self {
        Self {
            cwd: crate::resources::paths::absolute(&options.cwd),
            agent_dir: crate::resources::paths::absolute(&options.agent_dir),
            home: options.home,
            settings: options.settings,
            runner: options.runner,
            builtin_extensions: options.builtin_extensions,
            http_client: options.http_client,
            offline: options.offline,
            progress: Mutex::new(None),
            global_npm_root: Mutex::new(None),
        }
    }

    pub fn settings(&self) -> &Arc<SettingsManager> {
        &self.settings
    }

    pub fn set_progress_callback(&self, callback: Option<ProgressCallback>) {
        *self.progress.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = callback;
    }

    pub(crate) fn is_offline(&self) -> bool {
        self.offline.unwrap_or_else(is_offline_mode_enabled)
    }

    pub(crate) fn http(&self) -> reqwest::Client {
        self.http_client.clone().unwrap_or_else(pi_ai::http::shared_client)
    }

    fn emit_progress(&self, event: ProgressEvent) {
        let callback = self.progress.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
        if let Some(callback) = callback {
            callback(&event);
        }
    }

    /// Emit `start`, then `complete` or `error` around `operation`.
    pub(crate) async fn with_progress<T>(
        &self,
        action: ProgressAction,
        source: &str,
        message: String,
        operation: impl Future<Output = anyhow::Result<T>>,
    ) -> anyhow::Result<T> {
        let event = |kind, message| ProgressEvent { kind, action, source: source.to_string(), message };
        self.emit_progress(event(ProgressKind::Start, Some(message)));
        match operation.await {
            Ok(value) => {
                self.emit_progress(event(ProgressKind::Complete, None));
                Ok(value)
            }
            Err(error) => {
                self.emit_progress(event(ProgressKind::Error, Some(error.to_string())));
                Err(error)
            }
        }
    }
}

/// `PI_OFFLINE` set to `1`, `true`, or `yes` (also set by `--offline`).
pub fn is_offline_mode_enabled() -> bool {
    crate::config::env_flag("PI_OFFLINE")
}

/// The pi installation directory: `PI_PACKAGE_DIR` (for Nix or Guix store paths), else the
/// directory of the running executable, as upstream does for a compiled binary.
pub fn package_dir() -> PathBuf {
    let exe = std::env::current_exe().ok();
    package_dir_from(pi_ai::env::process_env_value("PI_PACKAGE_DIR").as_deref(), exe.as_deref())
}

pub fn package_dir_from(env_value: Option<&str>, exe: Option<&Path>) -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match env_value.filter(|value| !value.is_empty()) {
        Some(value) => crate::resources::paths::resolve_path(value, &cwd),
        None => exe.and_then(Path::parent).map(Path::to_path_buf).unwrap_or(cwd),
    }
}

pub(crate) fn settings_scope(scope: SourceScope) -> SettingsScope {
    if scope == SourceScope::Project { SettingsScope::Project } else { SettingsScope::Global }
}

/// Run `tasks` with at most `limit` in flight; results keep the input order.
pub(crate) async fn run_with_concurrency<T>(tasks: Vec<BoxFuture<'_, T>>, limit: usize) -> Vec<T> {
    use futures::StreamExt;
    futures::stream::iter(tasks).buffered(limit.max(1)).collect().await
}
