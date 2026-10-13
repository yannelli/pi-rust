//! Self-update for release installs (`pi update`): find the newest GitHub release of
//! pi-rust, download the archive for this target, check it against `SHA256SUMS`, and replace
//! the running binary. Cargo installs are pointed at `cargo install` instead.

pub mod install;
pub mod release;

#[cfg(test)]
mod tests;

use std::path::PathBuf;

use tokio_util::sync::CancellationToken;

pub use install::{
    CARGO_UPDATE_COMMAND, InstallMethod, QUARANTINE_DIR_NAME, cleanup_update_quarantine, detect_install_method,
    extract_binary, parse_sha256sums, replace_executable, sha256_hex, verify_checksum,
};
pub use release::{
    ArchiveKind, GITHUB_REPO, GitHubRelease, ReleaseAsset, ReleaseSource, UpdateError, archive_name, binary_name,
    github_api_url, release_target, select_assets,
};

use crate::platform::version_check::is_newer_package_version;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelfUpdatePlan {
    pub current_version: String,
    pub release: GitHubRelease,
    /// Newer than the running version, or forced.
    pub should_run: bool,
}

impl SelfUpdatePlan {
    pub fn latest_version(&self) -> &str {
        self.release.version()
    }

    /// Release notes to show before updating.
    pub fn notes(&self) -> Option<&str> {
        self.release.body.as_deref()
    }
}

pub async fn plan_self_update(
    source: &ReleaseSource,
    current_version: &str,
    force: bool,
    cancel: &CancellationToken,
) -> Result<SelfUpdatePlan, UpdateError> {
    let release = source.latest_release(cancel).await.map_err(|error| match error {
        UpdateError::Cancelled => UpdateError::Cancelled,
        other => UpdateError::Network(format!("Could not determine latest pi version: {other}")),
    })?;
    let should_run = force || is_newer_package_version(release.version(), current_version);
    Ok(SelfUpdatePlan { current_version: current_version.into(), release, should_run })
}

#[derive(Clone, Debug)]
pub struct SelfUpdateOptions {
    pub source: ReleaseSource,
    pub current_version: String,
    /// Reinstall even when the running version is the latest.
    pub force: bool,
    pub executable: PathBuf,
    /// Release target triple; `None` on platforms without release archives.
    pub target: Option<String>,
    pub cargo_home: Option<PathBuf>,
    pub home: PathBuf,
    pub cancel: CancellationToken,
}

impl SelfUpdateOptions {
    /// Options for the running binary.
    pub fn for_current_process(force: bool) -> std::io::Result<Self> {
        Ok(Self {
            source: ReleaseSource::default(),
            current_version: crate::platform::VERSION.into(),
            force,
            executable: std::env::current_exe()?,
            target: release_target().map(String::from),
            cargo_home: std::env::var_os("CARGO_HOME").filter(|home| !home.is_empty()).map(PathBuf::from),
            home: crate::config::home_dir(),
            cancel: CancellationToken::new(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelfUpdateOutcome {
    UpToDate { version: String },
    Updated { from: String, to: String, executable: PathBuf, notes: Option<String> },
}

impl SelfUpdateOutcome {
    /// The line `pi update` prints.
    pub fn message(&self) -> String {
        match self {
            Self::UpToDate { version } => format!("pi is already up to date (v{version})"),
            Self::Updated { from, to, .. } => format!("Updated pi from {from} to {to}"),
        }
    }
}

/// Update the binary at `options.executable` to the latest release.
pub async fn run_self_update(options: &SelfUpdateOptions) -> Result<SelfUpdateOutcome, UpdateError> {
    let method = detect_install_method(&options.executable, options.cargo_home.as_deref(), &options.home);
    if method == InstallMethod::Cargo {
        return Err(UpdateError::Unsupported(format!(
            "pi was installed with cargo. Update it with: {CARGO_UPDATE_COMMAND}"
        )));
    }
    let Some(target) = options.target.as_deref() else {
        return Err(UpdateError::Unsupported("No pi release archive is published for this platform.".into()));
    };
    let plan = plan_self_update(&options.source, &options.current_version, options.force, &options.cancel).await?;
    if !plan.should_run {
        return Ok(SelfUpdateOutcome::UpToDate { version: options.current_version.clone() });
    }
    let (archive, kind, sums) = select_assets(&plan.release, target)?;
    let archive_bytes = options.source.download(&archive.url, "application/octet-stream", &options.cancel).await?;
    let sums_bytes = options.source.download(&sums.url, "application/octet-stream", &options.cancel).await?;
    verify_checksum(&String::from_utf8_lossy(&sums_bytes), &archive.name, &archive_bytes)?;
    let binary = extract_binary(&archive_bytes, kind, binary_name(target))?;
    if binary.is_empty() {
        return Err(UpdateError::Release(format!("{} is empty in {}", binary_name(target), archive.name)));
    }
    if options.cancel.is_cancelled() {
        return Err(UpdateError::Cancelled);
    }
    replace_executable(&options.executable, &binary)?;
    Ok(SelfUpdateOutcome::Updated {
        from: options.current_version.clone(),
        to: plan.latest_version().to_string(),
        executable: options.executable.clone(),
        notes: plan.notes().map(String::from),
    })
}
