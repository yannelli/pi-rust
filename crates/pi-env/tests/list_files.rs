//! Durable's `listFiles` conformance cases (upstream `env-conformance.ts`), run against `RemoteExecutionEnv` and
//! `LocalExecutionEnv`: both list through the daemon, so they must list the same.

mod support;

use std::sync::Arc;

use pi_durable::env::LocalEnvOptions;
#[cfg(unix)]
use pi_durable::testing::env_conformance::EnvConformanceOptions;
use pi_env::LocalExecutionEnv;
use support::list_cases;
use support::{Guard, environment};

fn local() -> (Arc<LocalExecutionEnv>, Guard) {
    let dir = tempfile::Builder::new().prefix("pi-env-local-").tempdir().expect("temp dir");
    let connection = support::connection();
    let cwd = dir.path().to_string_lossy().into_owned();
    let env = Arc::new(LocalExecutionEnv::new(LocalEnvOptions::new(cwd), connection.clone()));
    (env, Guard { dir, connection })
}

macro_rules! cases {
    ($setup:expr) => {
        use super::*;

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn lists_files_and_directories_depth_first_in_name_order() {
            let (env, _guard) = $setup;
            list_cases::lists_files_and_directories_depth_first_in_name_order(env.as_ref()).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn skips_what_git_ignores_in_work_trees_git_and_excluded_names() {
            let (env, _guard) = $setup;
            list_cases::skips_what_git_ignores_in_work_trees_git_and_excluded_names(env.as_ref()).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn stops_at_its_bounds_and_reports_truncated() {
            let (env, _guard) = $setup;
            list_cases::stops_at_its_bounds_and_reports_truncated(env.as_ref()).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn fails_on_missing_directories_files_and_aborts() {
            let (env, _guard) = $setup;
            list_cases::fails_on_missing_directories_files_and_aborts(env.as_ref()).await;
        }

        #[cfg(unix)]
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn lists_symbolic_links_without_following_them() {
            let (env, _guard) = $setup;
            list_cases::lists_symbolic_links_without_following_them(
                env.as_ref(),
                &EnvConformanceOptions::default().shell,
            )
            .await;
        }
    };
}

mod remote {
    cases!(environment());
}

mod through_local_env {
    cases!(local());
}
