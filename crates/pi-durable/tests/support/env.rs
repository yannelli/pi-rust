//! Helpers of the execution environment tests.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use pi_chord::{AbortController, BACKGROUND_CONTEXT, Context, with_abort_signal};
use pi_durable::env::{LocalEnvOptions, LocalExecutionEnv};

pub fn context() -> Context {
    BACKGROUND_CONTEXT.clone()
}

pub fn aborted_context() -> Context {
    let controller = AbortController::new();
    controller.abort(None);
    with_abort_signal(&controller.signal(), &BACKGROUND_CONTEXT)
}

/// A context and the controller that aborts it.
pub fn abortable() -> (Context, AbortController) {
    let controller = AbortController::new();
    (with_abort_signal(&controller.signal(), &BACKGROUND_CONTEXT), controller)
}

pub fn temp_dir() -> tempfile::TempDir {
    tempfile::Builder::new().prefix("pi-durable-env-").tempdir().unwrap()
}

pub fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

pub fn join(root: &Path, relative: &str) -> String {
    path_string(&root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR)))
}

pub fn local_env(root: &Path) -> LocalExecutionEnv {
    LocalExecutionEnv::new(LocalEnvOptions::new(path_string(root)))
}

/// Remove the directory holding a spill or temporary file.
pub fn remove_parent(path: &str) {
    if let Some(parent) = PathBuf::from(path).parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

pub async fn wait_for(mut condition: impl AsyncFnMut() -> bool, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    while !condition().await {
        assert!(tokio::time::Instant::now() < deadline, "condition not met within {timeout:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
