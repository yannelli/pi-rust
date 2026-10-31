//! `LocalFileWatcher`: watches by snapshots. Native events only trigger a debounced rescan, and changes are the
//! difference between snapshots plus the event paths, so replaced files, renamed ancestors, and directories created
//! with their contents never depend on which events the operating system sends.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use pi_chord::Context;
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::LocalWatchOptions;
use super::paths::blocking;
use super::watch_scan::{
    EntryKind, ResolvedTarget, Scan, ScanError, Snapshot, ancestors_of, any_unreliable, diff, in_scope, is_within,
    reported, scan,
};
use crate::env::{
    FileChange, FileError, FileErrorCode, FileResult, FileWatcher, WatchListener, WatchMode, WatchTarget,
};

const DEBOUNCE: Duration = Duration::from_millis(50);
/// On macOS, an FSEvents stream can miss changes made right after it starts; a rescan this long after installing
/// watchers catches them.
const FSEVENTS_SETTLE: Duration = Duration::from_millis(500);
const DEFAULT_POLL_MS: u64 = 2000;
const DEFAULT_MAX_DIRECTORIES: usize = 10_000;
const MAX_SYNC_ROUNDS: usize = 10;

enum Message {
    /// Native event paths; `removed` when the event removed them, which drops the backend's watch on each.
    Event {
        paths: Vec<PathBuf>,
        removed: bool,
    },
    Rescan,
    Close,
}

/// Delivery state shared with the handle: once `closed` is set under its lock, no listener call starts.
struct Shared {
    closed: Mutex<bool>,
    on_change: WatchListener,
    mode: Mutex<WatchMode>,
}

impl Shared {
    fn deliver(&self, change: FileChange) {
        let closed = self.closed.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if *closed {
            return;
        }
        // A panicking listener must not stop watching.
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| (self.on_change)(change)));
    }

    fn is_closed(&self) -> bool {
        *self.closed.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn close(&self) {
        *self.closed.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
    }

    fn mode(&self) -> WatchMode {
        *self.mode.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// A running watch of a `LocalExecutionEnv`.
pub struct LocalFileWatcher {
    shared: Arc<Shared>,
    messages: mpsc::UnboundedSender<Message>,
    task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

struct Core {
    targets: Arc<Vec<ResolvedTarget>>,
    shared: Arc<Shared>,
    messages: mpsc::UnboundedSender<Message>,
    poll_interval: Duration,
    max_directories: usize,
    snapshot: Snapshot,
    watcher: Option<RecommendedWatcher>,
    /// Watched paths and the identity of what each watch was installed on.
    installed: HashMap<String, (u64, u64)>,
    /// Real paths of watched paths that differ from the given spelling. FSEvents reports real paths, such as
    /// `/private/var` for `/var` on macOS.
    aliases: Vec<(String, String)>,
    events: BTreeSet<String>,
    flush_at: Option<Instant>,
    settle_at: Option<Instant>,
    poll_at: Option<Instant>,
    stopped: bool,
}

fn scan_error(error: ScanError) -> FileError {
    match error {
        ScanError::Budget(message) => FileError::new(FileErrorCode::Invalid, message, None),
        ScanError::Io(error) => {
            let code = if error.kind() == std::io::ErrorKind::PermissionDenied {
                FileErrorCode::PermissionDenied
            } else {
                FileErrorCode::Invalid
            };
            FileError::new(code, error.to_string(), None).with_cause(Arc::new(error))
        }
    }
}

fn is_missing_or_denied(error: &notify::Error) -> bool {
    match &error.kind {
        notify::ErrorKind::PathNotFound | notify::ErrorKind::WatchNotFound => true,
        notify::ErrorKind::Io(io) => {
            matches!(io.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied)
        }
        _ => false,
    }
}

impl Core {
    fn is_polling(&self) -> bool {
        self.shared.mode() == WatchMode::Polling
    }

    async fn scan(&self) -> Result<Scan, ScanError> {
        let (targets, polling, max) = (self.targets.clone(), self.is_polling(), self.max_directories);
        blocking(move || scan(&targets, polling, max)).await
    }

    /// Rescan, report differences, and install watchers for new directories, rescanning until none are new.
    async fn sync(&mut self, mut report: bool) -> Result<BTreeSet<String>, ScanError> {
        let mut changed = BTreeSet::new();
        for _ in 0..MAX_SYNC_ROUNDS {
            if self.shared.is_closed() {
                break;
            }
            let scan = self.scan().await?;
            // Closed during the scan: installing watchers now would leak them.
            if self.shared.is_closed() {
                break;
            }
            if report {
                changed.extend(diff(&self.snapshot, &scan.snapshot, &self.targets));
            }
            self.snapshot = scan.snapshot;
            if self.is_polling() || !self.reconcile_watchers(&scan.linked_files) {
                break;
            }
            if cfg!(target_os = "macos") {
                self.settle_at = Some(Instant::now() + FSEVENTS_SETTLE);
            }
            // Something written into a new directory before its watcher existed shows up in the next round.
            report = true;
        }
        Ok(changed)
    }

    /// The paths to watch, each with whether its watch is recursive.
    fn wanted_watches(&self, linked_files: &HashSet<String>) -> Vec<(String, bool)> {
        let mut wanted: Vec<(String, bool)> = linked_files.iter().map(|path| (path.clone(), false)).collect();
        let per_directory = cfg!(any(target_os = "linux", target_os = "android"));
        let is_directory = |path: &str| self.snapshot.get(path).is_some_and(|entry| entry.kind == EntryKind::Directory);
        let add = |wanted: &mut Vec<(String, bool)>, path: &str, recursive: bool| match wanted
            .iter_mut()
            .find(|(existing, _)| existing == path)
        {
            Some(entry) => entry.1 |= recursive,
            None => wanted.push((path.to_string(), recursive)),
        };
        for target in self.targets.iter() {
            for ancestor in ancestors_of(&target.path) {
                if is_directory(&ancestor) {
                    add(&mut wanted, &ancestor, false);
                }
            }
            if !is_directory(&target.path) {
                continue;
            }
            add(&mut wanted, &target.path, target.recursive && !per_directory);
            if target.recursive && per_directory {
                for (path, entry) in &self.snapshot {
                    if entry.kind == EntryKind::Directory && path != &target.path && is_within(path, &target.path) {
                        add(&mut wanted, path, false);
                    }
                }
            }
        }
        wanted
    }

    /// Watch every existing ancestor of each target, each target directory, each target that links to a file, and on
    /// Linux each directory below a recursive target (elsewhere one recursive watch per target). Returns whether a
    /// watch was added.
    fn reconcile_watchers(&mut self, linked_files: &HashSet<String>) -> bool {
        let wanted = self.wanted_watches(linked_files);
        // Gone, or replaced: a watch follows the directory it was installed on.
        let stale: Vec<String> = self
            .installed
            .iter()
            .filter(|(path, identity)| {
                let current = self.snapshot.get(*path).map(|entry| (entry.dev, entry.ino));
                !wanted.iter().any(|(wanted, _)| wanted == *path) || current != Some(**identity)
            })
            .map(|(path, _)| path.clone())
            .collect();
        for path in stale {
            self.installed.remove(&path);
            if let Some(watcher) = self.watcher.as_mut() {
                let _ = watcher.unwatch(Path::new(&path));
            }
        }
        let mut added = false;
        for (path, recursive) in wanted {
            let Some(identity) = self.snapshot.get(&path).map(|entry| (entry.dev, entry.ino)) else { continue };
            if self.installed.contains_key(&path) {
                continue;
            }
            if self.watcher.is_none() {
                let messages = self.messages.clone();
                let handler = move |event: notify::Result<notify::Event>| {
                    let message = match event {
                        Ok(event) if !event.need_rescan() => {
                            let removed = matches!(event.kind, notify::EventKind::Remove(_));
                            Message::Event { paths: event.paths, removed }
                        }
                        _ => Message::Rescan,
                    };
                    let _ = messages.send(message);
                };
                match RecommendedWatcher::new(handler, notify::Config::default()) {
                    Ok(watcher) => self.watcher = Some(watcher),
                    Err(_) => {
                        self.switch_to_polling();
                        return false;
                    }
                }
            }
            let mode = if recursive { RecursiveMode::Recursive } else { RecursiveMode::NonRecursive };
            let Some(watcher) = self.watcher.as_mut() else { return false };
            match watcher.watch(Path::new(&path), mode) {
                Ok(()) => {
                    if let Ok(real) = std::fs::canonicalize(&path).map(|real| real.to_string_lossy().into_owned())
                        && real != path
                        && !self.aliases.iter().any(|(existing, _)| *existing == real)
                    {
                        self.aliases.push((real, path.clone()));
                    }
                    self.installed.insert(path, identity);
                    added = true;
                }
                Err(error) if is_missing_or_denied(&error) => {}
                // Out of watches or unsupported: compare snapshots from now on, and say coverage was uncertain.
                Err(_) => {
                    self.switch_to_polling();
                    return false;
                }
            }
        }
        added
    }

    fn switch_to_polling(&mut self) {
        if self.is_polling() {
            return;
        }
        *self.shared.mode.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = WatchMode::Polling;
        self.watcher = None;
        self.installed.clear();
        self.shared.deliver(FileChange::Overflow);
        self.flush_at = None;
        self.poll_at = Some(Instant::now() + self.poll_interval);
    }

    fn on_event(&mut self, paths: Vec<PathBuf>, removed: bool) {
        if paths.is_empty() {
            self.schedule_flush();
        }
        for path in paths {
            let path = self.given_spelling(path.to_string_lossy().into_owned());
            // The backend dropped the watch of a watched path that was removed (inotify's IN_DELETE_SELF, then
            // IN_IGNORED). Forget it, so the rescan watches the path again even when its inode is the same.
            if removed {
                self.forget_watch(&path);
            }
            // Events about unrelated siblings of an ancestor, or about excluded entries, are ignored.
            if in_scope(&path, &self.targets) {
                self.events.insert(reported(&path, &self.targets));
                self.schedule_flush();
            }
        }
    }

    /// Forget the watch of `path`, if it has one, and remove it from the backend in case the backend kept it.
    fn forget_watch(&mut self, path: &str) {
        if self.installed.remove(path).is_some()
            && let Some(watcher) = self.watcher.as_mut()
        {
            let _ = watcher.unwatch(Path::new(path));
        }
    }

    /// Map a real path back to the spelling the targets use, by the longest matching alias.
    fn given_spelling(&self, path: String) -> String {
        let alias = self.aliases.iter().filter(|(real, _)| is_within(&path, real)).max_by_key(|(real, _)| real.len());
        match alias {
            Some((real, given)) => format!("{given}{}", &path[real.len()..]),
            None => path,
        }
    }

    fn schedule_flush(&mut self) {
        if self.flush_at.is_none() && !self.is_polling() {
            self.flush_at = Some(Instant::now() + DEBOUNCE);
        }
    }

    async fn flush(&mut self) {
        let events = std::mem::take(&mut self.events);
        match self.sync(true).await {
            Ok(mut changed) => {
                changed.extend(events);
                if !changed.is_empty() {
                    self.shared.deliver(FileChange::Paths(changed.into_iter().collect()));
                }
            }
            Err(error) => {
                self.shared.deliver(FileChange::Failed(scan_error(error)));
                self.shared.close();
                self.stopped = true;
            }
        }
    }

    async fn run(mut self, mut messages: mpsc::UnboundedReceiver<Message>) {
        while !self.stopped && !self.shared.is_closed() {
            let next = [self.flush_at, self.settle_at, self.poll_at].into_iter().flatten().min();
            tokio::select! {
                message = messages.recv() => match message {
                    None | Some(Message::Close) => break,
                    Some(Message::Event { paths, removed }) => self.on_event(paths, removed),
                    Some(Message::Rescan) => self.schedule_flush(),
                },
                () = tokio::time::sleep_until(next.unwrap_or_else(Instant::now)), if next.is_some() => {
                    let now = Instant::now();
                    let due = |at: Option<Instant>| at.is_some_and(|at| at <= now);
                    let polled = due(self.poll_at);
                    if due(self.flush_at) {
                        self.flush_at = None;
                    }
                    if due(self.settle_at) {
                        self.settle_at = None;
                    }
                    if polled {
                        self.poll_at = None;
                    }
                    self.flush().await;
                    if polled && self.is_polling() {
                        self.poll_at = Some(Instant::now() + self.poll_interval);
                    }
                }
            }
        }
        self.watcher = None;
    }
}

impl LocalFileWatcher {
    /// Establish coverage: watches first, then the snapshot later changes are compared with. `targets` hold absolute
    /// paths.
    pub(crate) async fn open(
        targets: Vec<WatchTarget>,
        on_change: WatchListener,
        options: LocalWatchOptions,
    ) -> FileResult<Self> {
        let targets: Vec<ResolvedTarget> = targets.iter().map(ResolvedTarget::new).collect();
        // Windows refuses to rename a directory while another directory below it is open, and a native watch keeps
        // every watched directory open: watching would break renames of their parents, so Windows polls.
        let mode = match options.mode {
            Some(mode) => mode,
            None => {
                let paths: Vec<String> = targets.iter().map(|target| target.path.clone()).collect();
                let unreliable = blocking(move || any_unreliable(&paths)).await;
                if cfg!(windows) || unreliable { WatchMode::Polling } else { WatchMode::Native }
            }
        };
        let shared = Arc::new(Shared { closed: Mutex::new(false), on_change, mode: Mutex::new(mode) });
        let (sender, receiver) = mpsc::unbounded_channel();
        let poll_interval = Duration::from_millis(options.poll_interval_ms.unwrap_or(DEFAULT_POLL_MS));
        let mut core = Core {
            targets: Arc::new(targets),
            shared: shared.clone(),
            messages: sender.clone(),
            poll_interval,
            max_directories: options.max_directories.unwrap_or(DEFAULT_MAX_DIRECTORIES),
            snapshot: Snapshot::new(),
            watcher: None,
            installed: HashMap::new(),
            aliases: Vec::new(),
            events: BTreeSet::new(),
            flush_at: None,
            settle_at: None,
            poll_at: None,
            stopped: false,
        };
        if let Err(error) = core.sync(false).await {
            return Err(scan_error(error));
        }
        if core.is_polling() {
            core.poll_at = Some(Instant::now() + poll_interval);
        }
        let task = tokio::spawn(core.run(receiver));
        Ok(Self { shared, messages: sender, task: tokio::sync::Mutex::new(Some(task)) })
    }
}

#[async_trait]
impl FileWatcher for LocalFileWatcher {
    fn mode(&self) -> WatchMode {
        self.shared.mode()
    }

    async fn stop(&self, _context: &Context) {
        self.shared.close();
        let _ = self.messages.send(Message::Close);
        if let Some(task) = self.task.lock().await.take() {
            let _ = task.await;
        }
    }
}

impl Drop for LocalFileWatcher {
    fn drop(&mut self) {
        self.shared.close();
        let _ = self.messages.send(Message::Close);
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "watch_tests.rs"]
mod tests;
