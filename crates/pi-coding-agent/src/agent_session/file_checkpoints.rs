//! Checkpoints of the files the edit and write tools change, one group per prompt, for `/undo`
//! (pi-rust extension). They live in memory, belong to one session, and are lost on exit.

use std::collections::VecDeque;
use std::hash::{DefaultHasher, Hasher};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use super::{AgentSession, lock};
use crate::tools::{FileChangeRecorder, PriorContent, SharedFileChangeRecorder, too_large_reason};

/// Prompts whose changes `/undo` can restore.
pub const MAX_UNDO_PROMPTS: usize = 20;
/// Files larger than this before a change are not kept.
pub const MAX_CHECKPOINT_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// Length and hash of the bytes a tool wrote, to notice later changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Fingerprint {
    len: u64,
    hash: u64,
}

impl Fingerprint {
    fn of(bytes: &[u8]) -> Self {
        let mut hasher = DefaultHasher::new();
        hasher.write(bytes);
        Self { len: bytes.len() as u64, hash: hasher.finish() }
    }

    /// [`Self::of`] the content of `path`, read in chunks.
    fn of_file(path: &Path) -> std::io::Result<Self> {
        let mut file = std::fs::File::open(path)?;
        let (mut hasher, mut len, mut buffer) = (DefaultHasher::new(), 0u64, vec![0u8; 64 * 1024]);
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                return Ok(Self { len, hash: hasher.finish() });
            }
            hasher.write(&buffer[..read]);
            len += read as u64;
        }
    }
}

/// What a change left in a file.
#[derive(Clone, Copy, Debug)]
pub enum Written<'a> {
    Bytes(&'a [u8]),
    /// The file as it is on disk now, which pi reads in chunks and does not keep.
    OnDisk,
    Deleted,
}

struct FileCheckpoint {
    key: PathBuf,
    path: PathBuf,
    prior: PriorContent,
    /// `None` when the change deleted the file.
    written: Option<Fingerprint>,
    /// Found by a workspace scan; the agent did not report the change.
    detected: bool,
}

/// One file that `/undo` would restore.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndoCandidate {
    pub path: PathBuf,
    /// Found by the workspace scan of an ACP turn, not reported by the agent; another program
    /// or the user may have made the change.
    pub detected: bool,
}

struct PromptChanges {
    prompt: u64,
    files: Vec<FileCheckpoint>,
}

#[derive(Default)]
struct Inner {
    session_id: String,
    prompt: u64,
    groups: VecDeque<PromptChanges>,
}

/// Why `/undo` left a file as it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UndoSkip {
    /// The file changed after the agent last wrote it.
    ChangedSince,
    /// Its prior state was not kept; the text says why.
    NotRestorable(String),
    /// A change that the agent did not report, left as is on request.
    Unreported,
    /// Restoring it failed.
    Failed(String),
}

/// What `/undo` did with the files of one prompt.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UndoReport {
    pub restored: Vec<PathBuf>,
    pub deleted: Vec<PathBuf>,
    pub skipped: Vec<(PathBuf, UndoSkip)>,
    /// Earlier prompts whose changes `/undo` can still restore.
    pub remaining: usize,
}

/// The checkpoints of one session.
pub struct FileCheckpoints {
    inner: Mutex<Inner>,
    max_prompts: usize,
    max_file_bytes: u64,
}

impl Default for FileCheckpoints {
    fn default() -> Self {
        Self::new(MAX_UNDO_PROMPTS, MAX_CHECKPOINT_FILE_BYTES)
    }
}

impl FileCheckpoints {
    pub fn new(max_prompts: usize, max_file_bytes: u64) -> Self {
        Self { inner: Mutex::default(), max_prompts: max_prompts.max(1), max_file_bytes }
    }

    pub fn max_file_bytes(&self) -> u64 {
        self.max_file_bytes
    }

    /// The state of `session_id`; checkpoints of another session are dropped.
    fn for_session(&self, session_id: &str) -> MutexGuard<'_, Inner> {
        let mut inner = lock(&self.inner);
        if inner.session_id != session_id {
            inner.groups.clear();
            inner.session_id = session_id.to_string();
        }
        inner
    }

    /// Start a prompt: later changes form a new group.
    pub fn begin_prompt(&self, session_id: &str) {
        self.for_session(session_id).prompt += 1;
    }

    /// Whether the current prompt has not changed `path` yet.
    pub fn wants_prior(&self, session_id: &str, path: &Path) -> bool {
        let key = checkpoint_key(path);
        let inner = self.for_session(session_id);
        let current = inner.groups.back().filter(|group| group.prompt == inner.prompt);
        !current.is_some_and(|group| group.files.iter().any(|file| file.key == key))
    }

    /// `path` now holds `written`. `prior` is its state before the first change of the prompt.
    pub fn record(&self, session_id: &str, path: &Path, prior: Option<PriorContent>, written: &[u8]) {
        self.record_change(session_id, path, prior, Written::Bytes(written), false);
    }

    /// [`Self::record`] for any change, including a delete. A `detected` change comes from a
    /// workspace scan; a reported change of the same file clears the flag.
    pub fn record_change(
        &self,
        session_id: &str,
        path: &Path,
        prior: Option<PriorContent>,
        written: Written<'_>,
        detected: bool,
    ) {
        let key = checkpoint_key(path);
        let written = match written {
            Written::Bytes(bytes) => Some(Fingerprint::of(bytes)),
            Written::OnDisk => match Fingerprint::of_file(path) {
                Ok(fingerprint) => Some(fingerprint),
                Err(_) => return,
            },
            Written::Deleted => None,
        };
        let mut inner = self.for_session(session_id);
        let prompt = inner.prompt;
        if inner.groups.back().is_none_or(|group| group.prompt != prompt) {
            inner.groups.push_back(PromptChanges { prompt, files: Vec::new() });
            while inner.groups.len() > self.max_prompts {
                inner.groups.pop_front();
            }
        }
        let Some(group) = inner.groups.back_mut() else { return };
        if let Some(file) = group.files.iter_mut().find(|file| file.key == key) {
            file.written = written;
            file.detected &= detected;
            return;
        }
        let prior = match prior {
            Some(PriorContent::Bytes(bytes)) if bytes.len() as u64 > self.max_file_bytes => {
                PriorContent::NotRestorable(too_large_reason(self.max_file_bytes))
            }
            Some(prior) => prior,
            None => PriorContent::NotRestorable("its earlier content was not captured".into()),
        };
        group.files.push(FileCheckpoint { key, path: path.to_path_buf(), prior, written, detected });
    }

    /// Prompts of `session_id` whose changes `/undo` can restore.
    pub fn prompt_count(&self, session_id: &str) -> usize {
        self.for_session(session_id).groups.len()
    }

    /// The files that [`Self::undo`] would restore.
    pub fn candidates(&self, session_id: &str) -> Vec<UndoCandidate> {
        let inner = self.for_session(session_id);
        let files = inner.groups.back().into_iter().flat_map(|group| &group.files);
        files.map(|file| UndoCandidate { path: file.path.clone(), detected: file.detected }).collect()
    }

    /// Restore the files of the newest prompt with changes. `None` when there is none.
    pub fn undo(&self, session_id: &str) -> Option<UndoReport> {
        self.undo_with(session_id, true)
    }

    /// [`Self::undo`]; without `detected`, changes the agent did not report stay as they are.
    pub fn undo_with(&self, session_id: &str, detected: bool) -> Option<UndoReport> {
        let (group, remaining) = {
            let mut inner = self.for_session(session_id);
            let group = inner.groups.pop_back()?;
            (group, inner.groups.len())
        };
        let mut report = UndoReport { remaining, ..UndoReport::default() };
        for file in group.files {
            match file.detected && !detected {
                true => report.skipped.push((file.path, UndoSkip::Unreported)),
                false => restore(file, &mut report),
            }
        }
        Some(report)
    }
}

/// The path with its folder resolved, so two spellings of one file share a checkpoint.
fn checkpoint_key(path: &Path) -> PathBuf {
    let resolved =
        path.parent().zip(path.file_name()).and_then(|(dir, name)| Some(dir.canonicalize().ok()?.join(name)));
    resolved.unwrap_or_else(|| path.to_path_buf())
}

/// Whether `path` still holds what the agent last wrote, or is still absent after a delete.
fn unchanged_since(path: &Path, written: Option<Fingerprint>) -> std::io::Result<bool> {
    let Some(written) = written else {
        return match std::fs::symlink_metadata(path) {
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(true),
            Err(error) => Err(error),
            Ok(_) => Ok(false),
        };
    };
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() != written.len => return Ok(false),
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    Ok(Fingerprint::of_file(path)? == written)
}

fn restore(file: FileCheckpoint, report: &mut UndoReport) {
    let FileCheckpoint { path, prior, written, .. } = file;
    let was_deleted = written.is_none();
    let prior = match prior {
        PriorContent::NotRestorable(reason) => return report.skipped.push((path, UndoSkip::NotRestorable(reason))),
        PriorContent::Bytes(bytes) => Some(bytes),
        PriorContent::Missing => None,
    };
    match unchanged_since(&path, written) {
        Ok(true) => {}
        Ok(false) => return report.skipped.push((path, UndoSkip::ChangedSince)),
        Err(error) => return report.skipped.push((path, UndoSkip::Failed(error.to_string()))),
    }
    let (result, done) = match prior {
        Some(bytes) => (write_restored(&path, &bytes, was_deleted), &mut report.restored),
        None => (std::fs::remove_file(&path), &mut report.deleted),
    };
    match result {
        Ok(()) => done.push(path),
        Err(error) => report.skipped.push((path, UndoSkip::Failed(error.to_string()))),
    }
}

/// Restores `bytes`; a deleted file gets its folder back first.
fn write_restored(path: &Path, bytes: &[u8], was_deleted: bool) -> std::io::Result<()> {
    if was_deleted && let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes)
}

/// Forwards the changes of the session's tools to its checkpoints.
struct SessionRecorder(Weak<AgentSession>);

impl FileChangeRecorder for SessionRecorder {
    fn wants_prior(&self, path: &Path) -> bool {
        let Some(session) = self.0.upgrade() else { return false };
        session.file_checkpoints.wants_prior(&session.session_id(), path)
    }

    fn max_prior_bytes(&self) -> u64 {
        self.0.upgrade().map_or(MAX_CHECKPOINT_FILE_BYTES, |session| session.file_checkpoints.max_file_bytes())
    }

    fn record(&self, path: &Path, prior: Option<PriorContent>, written: &[u8]) {
        if let Some(session) = self.0.upgrade() {
            session.file_checkpoints.record(&session.session_id(), path, prior, written);
        }
    }
}

/// The recorder the built-in edit and write tools of `session` report to.
pub(crate) fn session_recorder(session: Weak<AgentSession>) -> SharedFileChangeRecorder {
    Arc::new(SessionRecorder(session))
}

impl AgentSession {
    /// Restore the files the edit and write tools changed during the newest prompt that changed
    /// any (pi-rust extension behind `/undo`). Call it while no run is active. `None` when no
    /// prompt of this session changed files.
    pub fn undo_file_changes(&self) -> Option<UndoReport> {
        self.file_checkpoints.undo(&self.session_id())
    }

    /// [`Self::undo_file_changes`]; without `detected`, the changes that an ACP agent did not
    /// report stay as they are.
    pub fn undo_file_changes_with(&self, detected: bool) -> Option<UndoReport> {
        self.file_checkpoints.undo_with(&self.session_id(), detected)
    }

    /// The files that [`Self::undo_file_changes`] would restore.
    pub fn undo_candidates(&self) -> Vec<UndoCandidate> {
        self.file_checkpoints.candidates(&self.session_id())
    }

    /// Records a change of an ACP turn for `/undo` (see [`FileCheckpoints::record_change`]).
    pub(crate) fn record_file_change(&self, path: &Path, prior: PriorContent, written: Written<'_>, detected: bool) {
        self.file_checkpoints.record_change(&self.session_id(), path, Some(prior), written, detected);
    }

    /// Prompts whose file changes [`Self::undo_file_changes`] can restore.
    pub fn undoable_prompts(&self) -> usize {
        self.file_checkpoints.prompt_count(&self.session_id())
    }

    /// The recorder for built-in tools that run outside the session's tool set.
    pub(crate) fn file_change_recorder(&self) -> SharedFileChangeRecorder {
        session_recorder(self.weak.clone())
    }
}

#[cfg(test)]
#[path = "file_checkpoints_tests.rs"]
mod tests;
