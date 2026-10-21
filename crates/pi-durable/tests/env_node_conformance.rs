//! Port of upstream `test/env-node-conformance.test.ts`: the conformance suite against `LocalExecutionEnv`, with
//! native and polling watches, plus its watch limits and reader cases.

#[path = "support/env.rs"]
mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_durable::env::{
    FileErrorCode, FileSystem, LocalEnvOptions, LocalExecutionEnv, LocalWatchOptions, RemoveOptions, SharedEnv,
    WatchChange, WatchMode, WatchTarget,
};
use pi_durable::testing::env_conformance::EnvConformanceOptions;
use support::{context, local_env, path_string, temp_dir};

fn env_in(watch: LocalWatchOptions) -> (SharedEnv, tempfile::TempDir) {
    let dir = tempfile::Builder::new().prefix("pi-durable-env-conformance-").tempdir().unwrap();
    let env = LocalExecutionEnv::new(LocalEnvOptions { watch, ..LocalEnvOptions::new(path_string(dir.path())) });
    (Arc::new(env), dir)
}

/// Git Bash's `ln -s` copies instead of linking unless native symlinks are enabled.
fn options() -> EnvConformanceOptions {
    if cfg!(windows) {
        let root = std::env::var("ProgramFiles").unwrap_or_else(|_| "C:\\Program Files".into());
        EnvConformanceOptions { shell: vec![format!("{root}\\Git\\bin\\bash.exe"), "-c".into()], symlinks: false }
    } else {
        EnvConformanceOptions::default()
    }
}

mod native {
    pi_durable::env_conformance_tests!(super::env_in(Default::default()), super::options());
}

mod polling {
    use pi_durable::env::{LocalWatchOptions, WatchMode};
    const POLLING: LocalWatchOptions =
        LocalWatchOptions { mode: Some(WatchMode::Polling), poll_interval_ms: Some(100), max_directories: None };
    pi_durable::env_conformance_tests!(super::env_in(POLLING), super::options());
}

#[tokio::test]
async fn reports_list_files_as_not_supported_until_an_environment_implements_it() {
    let (env, _dir) = env_in(Default::default());
    let error = env.list_files(".", None, &context()).await.unwrap_err();
    assert_eq!(error.code, FileErrorCode::NotSupported);
}

#[tokio::test]
async fn refuses_a_tree_over_the_directory_budget_and_stops_with_an_error_when_one_grows_past_it() {
    let dir = temp_dir();
    let watch = LocalWatchOptions { max_directories: Some(3), ..Default::default() };
    let env = LocalExecutionEnv::new(LocalEnvOptions { watch, ..LocalEnvOptions::new(path_string(dir.path())) });
    let ctx = context();
    env.create_dir("tree/a/b", true, &ctx).await.unwrap();
    env.create_dir("tree/c", true, &ctx).await.unwrap();
    let tree = [WatchTarget { path: "tree".into(), recursive: true, exclude: None }];
    let refused = env.watch(&tree, Arc::new(|_| {}), &ctx).await;
    assert_eq!(refused.err().map(|error| error.code), Some(FileErrorCode::Invalid));

    env.remove("tree/c", RemoveOptions { recursive: true, force: false }, &ctx).await.unwrap();
    let changes = Arc::new(Mutex::new(Vec::new()));
    let sink = changes.clone();
    let watcher = env.watch(&tree, Arc::new(move |change| sink.lock().unwrap().push(change)), &ctx).await.unwrap();
    // Windows polls by default (see LocalFileWatcher::open).
    assert_eq!(watcher.mode(), if cfg!(windows) { WatchMode::Polling } else { WatchMode::Native });
    env.create_dir("tree/d", true, &ctx).await.unwrap();
    let errored = || changes.lock().unwrap().iter().any(|change| matches!(change, WatchChange::Error(_)));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while !errored() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    match changes.lock().unwrap().last() {
        Some(WatchChange::Error(error)) => assert_eq!(error.code, FileErrorCode::Invalid),
        other => panic!("expected an error, got {other:?}"),
    }
    watcher.close(&ctx).await;
}

#[tokio::test]
async fn reads_ranges_spanning_several_internal_chunks_exactly() {
    let dir = temp_dir();
    let bytes: Vec<u8> = (0..(5 * 1024 * 1024 / 2)).map(|index: usize| ((index * 31) % 251) as u8).collect();
    std::fs::write(dir.path().join("big.bin"), &bytes).unwrap();
    let env = local_env(dir.path());
    let reader = env.open_binary_reader("big.bin", false, &context()).await.unwrap();
    let all = reader.read(0, bytes.len() as u64 + 10, &context()).await.unwrap();
    assert!(all == bytes);
    let middle = reader.read(1024 * 1024 - 3, 7, &context()).await.unwrap();
    assert_eq!(middle, bytes[1024 * 1024 - 3..1024 * 1024 + 4]);
    reader.close(&context()).await;
}

#[cfg(unix)]
#[tokio::test]
async fn refuses_a_fifo_without_waiting_for_a_writer() {
    let dir = temp_dir();
    let status = std::process::Command::new("mkfifo").arg(dir.path().join("pipe")).status().unwrap();
    assert!(status.success());
    let result = local_env(dir.path()).open_binary_reader("pipe", false, &context()).await;
    assert_eq!(result.err().map(|error| error.code), Some(FileErrorCode::Invalid));
}
