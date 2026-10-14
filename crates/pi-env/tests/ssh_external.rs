//! Upstream `test/ssh-external.test.ts`: the conformance suite over a real SSH server, configured by CI through
//! `PI_ENV_SSH_HOST`, `PI_ENV_SSH_USER`, `PI_ENV_SSH_PORT`, `PI_ENV_SSH_KEY` and `PI_ENV_SSH_PROGRAM`. Every test is
//! ignored: it needs that server. Run them with `cargo test -p pi-env --test ssh_external -- --ignored`.

mod support;

use std::sync::Arc;

use pi_durable::env::{FileSystem, RemoveOptions, SharedEnv};
use pi_durable::testing::env_conformance::{self, EnvConformanceOptions};
use pi_env::{
    Connection, RemoteExecutionEnv, RemoteExecutionEnvOptions, RemotePlatform, RemoteWatchOptions, SshConnectOptions,
    SshTarget, accept_host_key, connect_ssh, deploy_daemon, detect_platform, scan_host_key,
};
use support::ctx;

/// Scan and accept the host key, detect, deploy, and connect, logging each stage so a hang shows where it is.
async fn connect(root: &tempfile::TempDir) -> (Connection, RemotePlatform) {
    let host = std::env::var("PI_ENV_SSH_HOST").expect("PI_ENV_SSH_HOST names the SSH server");
    let known_hosts = root.path().join("known_hosts").display().to_string();
    let target = SshTarget {
        user: std::env::var("PI_ENV_SSH_USER").ok(),
        port: std::env::var("PI_ENV_SSH_PORT").ok().map(|port| port.parse().expect("PI_ENV_SSH_PORT")),
        identity_file: std::env::var("PI_ENV_SSH_KEY").ok(),
        ssh: std::env::var("PI_ENV_SSH_PROGRAM").ok(),
        ..SshTarget::new(host, known_hosts, "pi-env-external")
    };
    let log = |text: &str| eprintln!("[ssh-external] {text}");
    log("scanning the host key");
    accept_host_key(&target, &scan_host_key(&target).await.expect("scan").lines).await.expect("accept");
    log("detecting the platform");
    let remote = detect_platform(&target).await.expect("detect");
    log(&format!("detected {remote:?}; deploying"));
    let daemon: std::path::PathBuf = support::daemon().into();
    log(&format!("deployed {}; connecting", deploy_daemon(&target, &remote, &daemon).await.expect("deploy")));
    let options = SshConnectOptions {
        binary: Some(daemon),
        on_log: Some(Arc::new(move |text: &str| eprintln!("[ssh-external] daemon: {}", text.trim()))),
        ..SshConnectOptions::new(target)
    };
    let (connection, remote) = connect_ssh(options).await.expect("connect");
    log(&format!("hello {:?}", connection.info().await.expect("hello")));
    (connection, remote)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real SSH server: set PI_ENV_SSH_HOST"]
async fn reaches_the_remote_system_through_its_default_shell() {
    let root = tempfile::tempdir().unwrap();
    let (connection, remote) = connect(&root).await;
    let info = connection.info().await.expect("info");
    let expected = if cfg!(target_os = "macos") { "macos" } else { std::env::consts::OS };
    assert_eq!(info.os, expected);
    if cfg!(windows) {
        let shell = std::env::var("PI_ENV_SSH_SHELL").unwrap_or_else(|_| "cmd".into());
        let detected = format!("{:?}", remote.shell.expect("shell")).to_lowercase();
        assert_eq!(detected, shell);
    }
    connection.close();
}

#[cfg(not(windows))]
fn options() -> EnvConformanceOptions {
    EnvConformanceOptions::default()
}

/// Git Bash's `ln -s` copies instead of linking unless native symlinks are enabled.
#[cfg(windows)]
fn options() -> EnvConformanceOptions {
    let program_files = std::env::var("ProgramFiles").unwrap_or_else(|_| "C:\\Program Files".into());
    EnvConformanceOptions { shell: vec![format!("{program_files}\\Git\\bin\\bash.exe"), "-c".into()], symlinks: false }
}

/// A fresh remote directory for one case, removed afterwards.
async fn with_env(case: impl AsyncFnOnce(SharedEnv)) {
    let root = tempfile::tempdir().unwrap();
    let (connection, _) = connect(&root).await;
    let home_dir = connection.info().await.expect("info").home.clone();
    let home = RemoteExecutionEnv::new(RemoteExecutionEnvOptions::new(connection.clone(), "pi-env:external", home_dir));
    let cwd = home.create_temp_dir(Some("pi-env-conformance-"), &ctx()).await.expect("temp dir");
    let mut options = RemoteExecutionEnvOptions::new(connection.clone(), "pi-env:external", cwd.clone());
    options.watch = RemoteWatchOptions { poll_interval_ms: Some(100), ..Default::default() };
    case(Arc::new(RemoteExecutionEnv::new(options))).await;
    let _ = home.remove(&cwd, RemoveOptions { recursive: true, force: true }, &ctx()).await;
    connection.close();
}

/// The conformance cases, each ignored like the case above.
macro_rules! external_cases {
    ($($case:ident),* $(,)?) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            #[ignore = "needs a real SSH server: set PI_ENV_SSH_HOST"]
            async fn $case() {
                with_env(async |env| env_conformance::$case(env, &options()).await).await;
            }
        )*
    };
}

external_cases!(
    binary_reader_reads_byte_ranges_of_the_opened_file,
    binary_reader_scans_lines_like_decoding_the_whole_file,
    binary_reader_keeps_reading_the_file_it_opened_after_a_rename,
    binary_reader_refuses_directories_missing_files_and_aborted_opens,
    directory_reader_pages_every_entry_exactly_once,
    directory_reader_reports_the_end_and_refuses_use_after_close,
    directory_reader_refuses_missing_paths_and_files,
    directory_reader_skips_entries_removed_during_enumeration,
    watch_reports_a_missing_files_creation_changes_replacement_and_removal,
    watch_reports_a_missing_target_whose_ancestors_are_created,
    watch_follows_directories_created_together_with_their_contents,
    watch_keeps_watching_a_path_whose_parent_is_renamed_and_recreated,
    watch_skips_excluded_entries_and_reports_a_rename_out_of_them,
    watch_keeps_recursive_coverage_where_a_non_recursive_target_overlaps,
    watch_follows_a_directory_replaced_at_the_same_path,
    watch_stops_reporting_once_closed,
    argv_exec_passes_arguments_to_the_program_without_shell_parsing,
    exec_reports_the_stream_of_every_chunk_in_both_forms,
    argv_exec_honors_cwd_and_exit_codes,
    argv_exec_reports_missing_programs_and_empty_argv_as_spawn_errors,
    windowed_exec_keeps_the_exact_tail_and_counts_what_it_skips,
    argv_exec_distinguishes_timeout_from_abort,
    binary_reader_follows_symlinks_unless_no_follow_refuses_the_final_one,
    watch_reports_changes_to_the_file_a_watched_symbolic_link_points_to,
);
