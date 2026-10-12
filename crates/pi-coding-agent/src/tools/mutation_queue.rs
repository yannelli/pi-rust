//! Serializes mutations that target the same file. Different files still run in parallel.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};

type FileLock = tokio::sync::Mutex<()>;

static LOCKS: LazyLock<Mutex<HashMap<PathBuf, Weak<FileLock>>>> = LazyLock::new(Default::default);

fn queue_key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn lock_for(path: &Path) -> Arc<FileLock> {
    let key = queue_key(path);
    let mut map = LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    map.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = map.get(&key).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(FileLock::new(()));
    map.insert(key, Arc::downgrade(&lock));
    lock
}

/// Run `f` while holding the per-file lock for `path`. Waiters are served in FIFO order.
pub async fn with_file_mutation_queue<F, Fut, T>(path: &Path, f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = T>,
{
    let lock = lock_for(path);
    let _guard = lock.lock().await;
    f().await
}
