//! Upstream `test/conformance.test.ts`: Durable's env conformance suite, with its `listFiles` cases, against
//! `RemoteExecutionEnv` over a pipe and against `LocalExecutionEnv`. Each case gets its own connection: a tokio test
//! owns its runtime, so one connection cannot serve every case.

mod support;

use std::sync::Arc;

use pi_durable::env::{LocalEnvOptions, LocalWatchOptions, SharedEnv};
use pi_durable::testing::env_conformance::EnvConformanceOptions;
use pi_env::{LocalExecutionEnv, RemoteExecutionEnv, RemoteExecutionEnvOptions, RemoteWatchOptions};

fn setup() -> (SharedEnv, support::Guard) {
    let dir = tempfile::Builder::new().prefix("pi-env-conformance-").tempdir().expect("temp dir");
    let connection = support::connection();
    let mut options =
        RemoteExecutionEnvOptions::new(connection.clone(), "pi-env:test", dir.path().to_string_lossy().into_owned());
    options.watch = RemoteWatchOptions { poll_interval_ms: Some(100), ..Default::default() };
    let env: SharedEnv = Arc::new(RemoteExecutionEnv::new(options));
    (env, support::Guard { dir, connection })
}

fn setup_local() -> (SharedEnv, support::Guard) {
    let dir = tempfile::Builder::new().prefix("pi-env-local-conformance-").tempdir().expect("temp dir");
    let connection = support::connection();
    let mut options = LocalEnvOptions::new(dir.path().to_string_lossy().into_owned());
    options.watch = LocalWatchOptions { poll_interval_ms: Some(100), ..Default::default() };
    let env: SharedEnv = Arc::new(LocalExecutionEnv::new(options, connection.clone()));
    (env, support::Guard { dir, connection })
}

#[cfg(not(windows))]
fn options() -> EnvConformanceOptions {
    EnvConformanceOptions::default()
}

/// Git Bash's `ln -s` copies instead of linking unless native symlinks are enabled.
#[cfg(windows)]
fn options() -> EnvConformanceOptions {
    let program_files = std::env::var("ProgramFiles").unwrap_or_else(|_| "C:\\Program Files".into());
    let git_bash = format!("{program_files}\\Git\\bin\\bash.exe");
    EnvConformanceOptions { shell: vec![git_bash, "-c".into()], symlinks: false }
}

pi_durable::env_conformance_tests!(setup(), options());
pi_durable::env_list_files_conformance_tests!(setup(), options());

mod local {
    use super::*;

    pi_durable::env_conformance_tests!(setup_local(), options());
    pi_durable::env_list_files_conformance_tests!(setup_local(), options());
}
