//! Serialization of `edit` and `write` mutations of one file within this process.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, LazyLock, Mutex};

use futures::future::BoxFuture;
use pi_chord::Context;

use super::thrown;
use crate::env::{ExecutionEnv, FileErrorCode};
use crate::errors::DurableResult;

type Queue = Arc<tokio::sync::Mutex<()>>;

/// One queue per file, keyed by file system id and canonical path; tokio's mutex admits waiters in FIFO order.
static QUEUES: LazyLock<Mutex<HashMap<String, Queue>>> = LazyLock::new(Mutex::default);

async fn mutation_key(env: &dyn ExecutionEnv, path: &str, context: &Context) -> DurableResult<String> {
    let absolute = env.absolute_path(path, context).await.map_err(thrown)?;
    Ok(format!("{}\0{}", env.id(), canonical(env, absolute, context).await?))
}

/// The canonical path; for a file that does not exist yet, its canonical parent joined with its name, so a `write`
/// that creates a file and a later mutation of it share one key even under a symlinked directory.
fn canonical<'a>(
    env: &'a dyn ExecutionEnv,
    absolute: String,
    context: &'a Context,
) -> BoxFuture<'a, DurableResult<String>> {
    Box::pin(async move {
        match env.canonical_path(&absolute, context).await {
            Ok(path) => return Ok(path),
            Err(error) if error.code == FileErrorCode::NotSupported => return Ok(absolute),
            Err(error) if error.code != FileErrorCode::NotFound => return Err(thrown(error)),
            Err(_) => {}
        }
        // The file system splits the path, so a name may contain characters that are separators elsewhere.
        let parent = env.join_path(&[absolute.clone(), "..".to_string()], context).await.map_err(thrown)?;
        if parent == absolute || !absolute.starts_with(&parent) {
            return Ok(absolute);
        }
        let skip = if parent.ends_with('/') || parent.ends_with('\\') { 0 } else { 1 };
        let name = absolute[parent.len() + skip..].to_string();
        let parent = canonical(env, parent, context).await?;
        env.join_path(&[parent, name], context).await.map_err(thrown)
    })
}

/// Drops the queue of a key once no call holds or waits on it, also when the call's future is dropped.
struct Release {
    key: String,
    queue: Option<Queue>,
}

impl Drop for Release {
    fn drop(&mut self) {
        let mut queues = QUEUES.lock().unwrap_or_else(|poison| poison.into_inner());
        if let Some(queue) = self.queue.take()
            && Arc::strong_count(&queue) == 2
        {
            queues.remove(&self.key);
        }
    }
}

/// Serialize `edit` and `write` mutations of one file within this process: same file system id and canonical path,
/// whichever environment object the call got. Other files and file systems never wait. Concurrent calls on one file
/// run in the order their keys resolve. Not a lock against `bash` or other processes.
pub async fn with_file_mutation_queue<T, F, Fut>(
    env: &dyn ExecutionEnv,
    path: &str,
    run: F,
    context: &Context,
) -> DurableResult<T>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = DurableResult<T>>,
{
    let key = mutation_key(env, path, context).await?;
    let queue = {
        let mut queues = QUEUES.lock().unwrap_or_else(|poison| poison.into_inner());
        queues.entry(key.clone()).or_default().clone()
    };
    let release = Release { key, queue: Some(queue) };
    let result = {
        let _guard = match release.queue.as_ref() {
            Some(queue) => Some(queue.lock().await),
            None => None,
        };
        run().await
    };
    drop(release);
    result
}
