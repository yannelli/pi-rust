//! The hook the edit and write tools report their file changes to, so a session can keep
//! checkpoints for `/undo` (pi-rust extension).

use std::future::Future;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::Arc;

use super::truncate::format_size;

/// The state of a file before a tool changed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PriorContent {
    /// The file did not exist.
    Missing,
    /// The file's bytes.
    Bytes(Vec<u8>),
    /// The prior state was not kept; the text says why.
    NotRestorable(String),
}

/// Receives each change the edit and write tools make.
pub trait FileChangeRecorder: Send + Sync {
    /// Whether the change needs the prior state of `path`: its first change in the current prompt.
    fn wants_prior(&self, path: &Path) -> bool;
    /// Files larger than this are recorded as not restorable.
    fn max_prior_bytes(&self) -> u64;
    /// `path` now holds `written`. `prior` is set when [`Self::wants_prior`] returned true.
    fn record(&self, path: &Path, prior: Option<PriorContent>, written: &[u8]);
}

pub type SharedFileChangeRecorder = Arc<dyn FileChangeRecorder>;

/// Why a file larger than `max` bytes was not kept.
pub fn too_large_reason(max: u64) -> String {
    format!("larger than {}, not kept", format_size(usize::try_from(max).unwrap_or(usize::MAX)))
}

/// Run `write`, which puts `content` at `path`, and report the change to `recorder` once the
/// write succeeds. `known` is the current content of the file when the caller already read it.
pub(crate) async fn write_recorded<F, Fut>(
    recorder: Option<&SharedFileChangeRecorder>,
    path: &Path,
    known: Option<&[u8]>,
    content: &str,
    write: F,
) -> anyhow::Result<()>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    let Some(recorder) = recorder else { return write().await };
    let prior = match recorder.wants_prior(path) {
        true => Some(prior_content(path, known, recorder.max_prior_bytes()).await),
        false => None,
    };
    write().await?;
    recorder.record(path, prior, content.as_bytes());
    Ok(())
}

async fn prior_content(path: &Path, known: Option<&[u8]>, max: u64) -> PriorContent {
    let keep = |bytes: Vec<u8>| match bytes.len() as u64 > max {
        true => PriorContent::NotRestorable(too_large_reason(max)),
        false => PriorContent::Bytes(bytes),
    };
    if let Some(bytes) = known {
        return keep(bytes.to_vec());
    }
    match tokio::fs::metadata(path).await {
        Err(error) if error.kind() == ErrorKind::NotFound => return PriorContent::Missing,
        Ok(meta) if meta.len() > max => return PriorContent::NotRestorable(too_large_reason(max)),
        _ => {}
    }
    match tokio::fs::read(path).await {
        Ok(bytes) => keep(bytes),
        Err(error) if error.kind() == ErrorKind::NotFound => PriorContent::Missing,
        Err(error) => PriorContent::NotRestorable(format!("could not read it before the change: {error}")),
    }
}
