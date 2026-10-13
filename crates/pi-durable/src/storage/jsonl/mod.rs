//! Portable JSONL implementation of the storage contract over the `FileSystem` capability (spec section 11.3).
//! `main.jsonl` holds one marker per commit; `doc-<id>.jsonl` and `task-<id>.jsonl` sidecars hold content and live
//! task replacements. Reads come from a detached `MemoryStorage` index rebuilt from confirmed records.

mod commit;
mod format;
mod local;
mod recover;
mod store;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use pi_chord::Context;

use super::memory::MemoryStorage;
use crate::env::{FileError, FileSystem};
use crate::errors::{DurableError, DurableResult};
use crate::ids::{DocumentId, TaskId};
use format::MAIN_FILE;
pub use local::open_local_jsonl_storage;

/// Options of a JSONL storage.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JsonlStorageOptions {
    /// Flush every affected sidecar before appending the main marker. Defaults to false.
    pub fsync: bool,
}

/// JSONL-backed `Storage`.
pub struct JsonlStorage {
    fs: Arc<dyn FileSystem>,
    directory: String,
    main_path: String,
    fsync: bool,
    memory: MemoryStorage,
    current_only_documents: Mutex<HashSet<DocumentId>>,
    live_task_sidecars: Mutex<HashSet<TaskId>>,
    commit_line: tokio::sync::Mutex<()>,
    closed: std::sync::atomic::AtomicBool,
    poison: Mutex<Option<DurableError>>,
}

pub(crate) fn error_from_file(action: &str, error: FileError) -> DurableError {
    DurableError::storage(format!("JSONL {action} failed: {}", error.message))
}

impl JsonlStorage {
    /// Open or create a JSONL storage directory on the supplied file system.
    pub async fn open(
        directory: &str,
        fs: Arc<dyn FileSystem>,
        context: &Context,
        options: JsonlStorageOptions,
    ) -> DurableResult<Self> {
        let absolute = fs.absolute_path(directory, context).await.map_err(|e| error_from_file("path resolution", e))?;
        fs.create_dir(&absolute, true, context).await.map_err(|e| error_from_file("directory creation", e))?;
        let parts = [absolute.clone(), MAIN_FILE.to_string()];
        let main_path = fs.join_path(&parts, context).await.map_err(|e| error_from_file("path join", e))?;
        let storage = Self {
            fs,
            directory: absolute,
            main_path,
            fsync: options.fsync,
            memory: MemoryStorage::new(),
            current_only_documents: Mutex::new(HashSet::new()),
            live_task_sidecars: Mutex::new(HashSet::new()),
            commit_line: tokio::sync::Mutex::new(()),
            closed: std::sync::atomic::AtomicBool::new(false),
            poison: Mutex::new(None),
        };
        storage.recover(context).await?;
        Ok(storage)
    }

    /// The directory this storage writes, as an absolute path.
    pub fn directory(&self) -> &str {
        &self.directory
    }

    fn assert_usable(&self) -> DurableResult<()> {
        if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(DurableError::storage("JsonlStorage is closed"));
        }
        if let Some(poison) = self.poison.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_ref() {
            return Err(poison.clone());
        }
        Ok(())
    }

    /// Poison the open backend after an uncertain append; every later call fails until it is reopened.
    fn poison(&self, cause: DurableError) -> DurableError {
        let mut poison = self.poison.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        poison
            .get_or_insert_with(|| {
                DurableError::storage(format!("JSONL storage is poisoned and must be reopened: {cause}"))
            })
            .clone()
    }

    async fn resolve_file(&self, file: &str, context: &Context) -> DurableResult<String> {
        let parts = [self.directory.clone(), file.to_string()];
        self.fs.join_path(&parts, context).await.map_err(|e| error_from_file("path join", e))
    }
}
