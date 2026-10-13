//! Process startup shared by every mode (upstream `main.ts`): the crash log hook, cleanup of
//! executables an update moved aside, `pi update --models` and `--self`, the startup changelog
//! with install telemetry, and the startup version check.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use futures::FutureExt;
use tokio::task::JoinHandle;

use crate::agent_session::{AgentSessionRuntime, catalog_base_url};
use crate::catalog::{CatalogConfig, refresh_model_catalogs};
use crate::diagnostics::{CrashContext, crash_log_path, install_crash_hook};
use crate::interactive::{InteractiveMode, InteractiveOptions};
use crate::packages::{ModelsUpdateFn, PackageCommandOptions, SelfUpdateFn, SelfUpdateRequest};
use crate::platform::VERSION;
use crate::platform::changelog::{StartupChangelog, embedded_changelog_entries, startup_changelog};
use crate::platform::telemetry::{REPORT_INSTALL_URL, install_telemetry_enabled, report_install_telemetry};
use crate::platform::version_check::{
    LatestPiRelease, VersionCheckOptions, check_for_new_pi_version, version_check_skipped,
};
use crate::resources::trust::{ProjectTrustStore, has_trust_requiring_project_resources};
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

/// `PI_STARTUP_BENCHMARK`: whether to time the interactive startup and exit.
pub fn startup_benchmark_enabled() -> bool {
    crate::config::env_flag("PI_STARTUP_BENCHMARK")
}

/// The `PI_STARTUP_BENCHMARK` run: set up the interactive UI, give the terminal 150ms to answer
/// its queries, restore the terminal, and print the timings with `interactiveMode.init`.
pub async fn run_startup_benchmark(
    runtime: Arc<AgentSessionRuntime>,
    options: InteractiveOptions,
) -> anyhow::Result<()> {
    let agent_dir = options.agent_dir.clone().unwrap_or_else(crate::config::agent_dir);
    crate::keybindings::KeybindingsManager::create(&agent_dir).install();
    let options = InteractiveOptions { agent_dir: Some(agent_dir), ..options };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            let mode = InteractiveMode::new(runtime, options);
            mode.init().await;
            crate::diagnostics::time("interactiveMode.init");
            let _ = tokio::time::timeout(Duration::from_millis(150), async {
                loop {
                    let _ = mode.run_once().await;
                }
            })
            .await;
            mode.stop(None);
            crate::theme::stop_theme_watcher();
            crate::diagnostics::print_timings();
            std::io::stdout().flush()?;
            std::io::stderr().flush()
        })
        .await?;
    Ok(())
}

/// Upstream `autoTrustOnReloadCwd`: the session cwd, when no `-a`/`-na` flag decides trust and
/// the project has no resources that need trust yet. A `/reload` that then finds such resources
/// saves the implicit trust.
pub fn auto_trust_on_reload_cwd(session_cwd: &Path, trust_override: Option<bool>, home: &Path) -> Option<PathBuf> {
    let needs_trust = has_trust_requiring_project_resources(session_cwd, home);
    (trust_override.is_none() && !needs_trust).then(|| session_cwd.to_path_buf())
}

/// Upstream `maybeSaveImplicitProjectTrustAfterReload`: after `/reload`, save trust for `cwd`
/// once when it is `auto_cwd`, trusted, newly has trust-requiring resources, and has no saved
/// decision. Returns whether trust was saved; the error is the warning to show.
pub fn save_implicit_project_trust_after_reload(
    auto_cwd: &mut Option<PathBuf>,
    cwd: &Path,
    project_trusted: bool,
    agent_dir: &Path,
    home: &Path,
) -> Result<bool, String> {
    if auto_cwd.as_deref() != Some(cwd) || !project_trusted || !has_trust_requiring_project_resources(cwd, home) {
        return Ok(false);
    }
    let store = ProjectTrustStore::new(agent_dir);
    let saved = store.get(cwd).and_then(|decision| match decision {
        Some(_) => Ok(false),
        None => store.set(cwd, Some(true)).map(|()| true),
    });
    match saved {
        Ok(saved) => {
            *auto_cwd = None;
            Ok(saved)
        }
        Err(error) => Err(format!("Could not save project trust after reload: {error:#}")),
    }
}

/// The `/reload` status line, noting saved project trust.
pub fn reload_status(saved_project_trust: bool) -> &'static str {
    if saved_project_trust {
        "Reloaded keybindings, extensions, skills, prompts, themes, and context files; saved project trust"
    } else {
        "Reloaded keybindings, extensions, skills, prompts, themes, and context files"
    }
}

#[cfg(test)]
#[path = "startup_tests.rs"]
mod tests;
