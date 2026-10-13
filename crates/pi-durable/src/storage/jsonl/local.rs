//! The JSONL opener over the local file system (upstream `node.ts`); the rest of the backend stays portable.

use std::sync::Arc;

use pi_chord::Context;

use super::{JsonlStorage, JsonlStorageOptions};
use crate::env::{LocalEnvOptions, LocalExecutionEnv};
use crate::errors::{DurableError, DurableResult};

/// Open or create a JSONL storage directory on the local file system (upstream `openNodeJsonlStorage`).
pub async fn open_local_jsonl_storage(
    directory: &str,
    context: &Context,
    options: JsonlStorageOptions,
) -> DurableResult<JsonlStorage> {
    let cwd = std::env::current_dir().map_err(|error| DurableError::storage(error.to_string()))?;
    let env = LocalExecutionEnv::new(LocalEnvOptions::new(cwd.to_string_lossy().into_owned()));
    JsonlStorage::open(directory, Arc::new(env), context, options).await
}
