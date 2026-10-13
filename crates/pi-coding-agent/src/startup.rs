//! Process startup shared by every mode (upstream `main.ts`): the crash log hook, cleanup of
//! executables an update moved aside, `pi update --models` and `--self`, the startup changelog
//! with install telemetry, and the startup version check.

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use futures::FutureExt;
use tokio::task::JoinHandle;

use crate::agent_session::catalog_base_url;
use crate::catalog::{CatalogConfig, refresh_model_catalogs};
use crate::diagnostics::{CrashContext, crash_log_path, install_crash_hook};
use crate::packages::{ModelsUpdateFn, PackageCommandOptions, SelfUpdateFn, SelfUpdateRequest};
use crate::platform::VERSION;
use crate::platform::changelog::{StartupChangelog, embedded_changelog_entries, startup_changelog};
use crate::platform::telemetry::{REPORT_INSTALL_URL, install_telemetry_enabled, report_install_telemetry};
use crate::platform::version_check::{
    LatestPiRelease, VersionCheckOptions, check_for_new_pi_version, version_check_skipped,
};
use crate::settings::SettingsManager;
use crate::update::{SelfUpdateOptions, UpdateError, cleanup_update_quarantine, run_self_update};

/// Record panics in the crash log of `agent_dir` and remove executables an update moved
/// aside. The returned context names the session a crash belongs to.
pub fn prepare_process(agent_dir: &Path) -> CrashContext {
    let context = CrashContext::default();
    if let Ok(cwd) = std::env::current_dir() {
        context.set_cwd(&cwd);
    }
    install_crash_hook(crash_log_path(agent_dir), context.clone());
    if let Ok(executable) = std::env::current_exe() {
        cleanup_update_quarantine(&executable);
    }
    context
}

/// Wire `pi update --models` and `pi update --self` into the package commands.
pub fn install_update_commands(options: &mut PackageCommandOptions) {
    options.models_update = Some(models_update_fn());
    options.self_update = Some(self_update_fn());
}

/// `pi update --models`: force-refresh every catalog into `models-store.json`, from
/// `PI_CATALOG_URL` when set.
pub fn models_update_fn() -> ModelsUpdateFn {
    Arc::new(|agent_dir| {
        async move {
            let config = CatalogConfig { catalog_base_url: catalog_base_url(), ..CatalogConfig::default() };
            refresh_model_catalogs(&agent_dir, &config).await.map(drop).map_err(anyhow::Error::msg)
        }
        .boxed()
    })
}

/// `pi update --self`: replace the running binary with the latest GitHub release.
pub fn self_update_fn() -> SelfUpdateFn {
    Arc::new(|request: SelfUpdateRequest| {
        async move {
            let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
            match SelfUpdateOptions::for_current_process(request.force) {
                Ok(options) => self_update_command(&options, &mut out, &mut err).await,
                Err(error) => {
                    let _ = writeln!(err, "Error: {error}");
                    1
                }
            }
        }
        .boxed()
    })
}

/// Run the self-update and print the outcome; returns the exit code. Cargo installs get the
/// `cargo install` command to run instead.
pub async fn self_update_command(
    options: &SelfUpdateOptions,
    out: &mut (dyn Write + Send),
    err: &mut (dyn Write + Send),
) -> i32 {
    match run_self_update(options).await {
        Ok(outcome) => {
            let _ = writeln!(out, "{}", outcome.message());
            0
        }
        Err(UpdateError::Unsupported(message)) => {
            let _ = writeln!(err, "error: pi cannot self-update this installation.");
            let _ = writeln!(err, "{message}");
            let _ = writeln!(err, "Installed at: {}", options.executable.display());
            1
        }
        Err(error) => {
            let _ = writeln!(err, "Error: {error}");
            1
        }
    }
}

static STARTUP_CHANGELOG: OnceLock<StartupChangelog> = OnceLock::new();

/// The startup changelog, computed once per process (upstream `getChangelogForDisplay`): on
/// an install or an update it stores the running version as `lastChangelogVersion` and sends
/// the install ping when telemetry is on. Later calls return the same result.
pub fn startup_changelog_once(settings: &SettingsManager, has_messages: bool) -> StartupChangelog {
    STARTUP_CHANGELOG
        .get_or_init(|| {
            let last = settings.last_changelog_version();
            let changelog = startup_changelog(&embedded_changelog_entries(), last.as_deref(), has_messages);
            if changelog.record_version {
                settings.set_last_changelog_version(VERSION);
            }
            if changelog.report_install {
                spawn_install_telemetry(settings);
            }
            changelog
        })
        .clone()
}

/// Send the anonymous install ping in the background when `enableInstallTelemetry` or
/// `PI_TELEMETRY` allows it and pi is online.
pub fn spawn_install_telemetry(settings: &SettingsManager) -> Option<JoinHandle<bool>> {
    if !install_telemetry_enabled(settings) || crate::config::offline() {
        return None;
    }
    let runtime = tokio::runtime::Handle::try_current().ok()?;
    Some(runtime.spawn(async {
        let client = pi_ai::http::shared_client();
        report_install_telemetry(&client, REPORT_INSTALL_URL, VERSION, true, false).await
    }))
}

/// The startup version check against the latest GitHub release; `PI_SKIP_VERSION_CHECK` and
/// `PI_OFFLINE` skip it. The task yields a newer release, if any.
pub fn spawn_version_check() -> Option<JoinHandle<Option<LatestPiRelease>>> {
    if version_check_skipped() || crate::config::offline() {
        return None;
    }
    Some(tokio::spawn(async { check_for_new_pi_version(VERSION, &VersionCheckOptions::default(), false).await }))
}

#[cfg(test)]
#[path = "startup_tests.rs"]
mod tests;
