//! Snapshots of watched trees and their differences, for `LocalFileWatcher`. Blocking; run off the async runtime.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hasher};
use std::io;
use std::path::{MAIN_SEPARATOR, Path};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::env::WatchTarget;

/// In polling mode, small files changed in the last few seconds are also compared by content: a second write within
/// the file system's timestamp granularity can keep size and timestamp.
const HASH_MAX_BYTES: u64 = 256 * 1024;
const HASH_RECENT_MS: u128 = 5000;

/// A watched path with its exclusions.
#[derive(Clone, Debug)]
pub(crate) struct ResolvedTarget {
    pub path: String,
    pub recursive: bool,
    pub hidden: bool,
    pub names: HashSet<String>,
}

impl ResolvedTarget {
    pub fn new(target: &WatchTarget) -> Self {
        let exclude = target.exclude.clone().unwrap_or_default();
        Self {
            path: target.path.clone(),
            recursive: target.recursive,
            hidden: exclude.hidden,
            names: exclude.names.into_iter().collect(),
        }
    }

    fn excluded(&self, name: &str) -> bool {
        (self.hidden && name.starts_with('.')) || self.names.contains(name)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

/// What a snapshot remembers of one path. Directories and ancestors are compared by identity only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub kind: EntryKind,
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_ns: u128,
    pub hash: Option<u64>,
}

pub(crate) type Snapshot = HashMap<String, Entry>;

/// A snapshot, and targets that are symbolic links to files, whose files need their own watchers.
#[derive(Debug, Default)]
pub(crate) struct Scan {
    pub snapshot: Snapshot,
    pub linked_files: HashSet<String>,
}

#[derive(Debug)]
pub(crate) enum ScanError {
    /// The watched tree has more directories than allowed.
    Budget(String),
    Io(io::Error),
}

impl From<io::Error> for ScanError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

fn since_epoch_ns(time: io::Result<SystemTime>) -> u128 {
    time.ok().and_then(|time| time.duration_since(UNIX_EPOCH).ok()).map_or(0, |duration| duration.as_nanos())
}

#[cfg(unix)]
fn identity(metadata: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}

/// Without stable inode numbers, a creation time stands in for the identity.
#[cfg(not(unix))]
fn identity(metadata: &std::fs::Metadata) -> (u64, u64) {
    (0, since_epoch_ns(metadata.created()) as u64)
}

pub(crate) fn entry_of(metadata: &std::fs::Metadata, hash: Option<u64>) -> Entry {
    let file_type = metadata.file_type();
    let kind = if file_type.is_file() {
        EntryKind::File
    } else if file_type.is_dir() {
        EntryKind::Directory
    } else if file_type.is_symlink() {
        EntryKind::Symlink
    } else {
        EntryKind::Other
    };
    let (dev, ino) = identity(metadata);
    let directory = kind == EntryKind::Directory;
    Entry {
        kind,
        dev,
        ino,
        size: if directory { 0 } else { metadata.len() },
        mtime_ns: if directory { 0 } else { since_epoch_ns(metadata.modified()) },
        hash,
    }
}

/// Ancestors of `path`, nearest first, up to the root.
pub(crate) fn ancestors_of(path: &str) -> Vec<String> {
    Path::new(path).ancestors().skip(1).map(|ancestor| ancestor.to_string_lossy().into_owned()).collect()
}

/// Whether `path` is `ancestor` or below it.
pub(crate) fn is_within(path: &str, ancestor: &str) -> bool {
    if path == ancestor {
        return true;
    }
    match path.strip_prefix(ancestor) {
        Some(rest) => ancestor.ends_with(MAIN_SEPARATOR) || rest.starts_with(MAIN_SEPARATOR),
        None => false,
    }
}

pub(crate) fn child_path(directory: &str, name: &str) -> String {
    if directory.ends_with(MAIN_SEPARATOR) {
        format!("{directory}{name}")
    } else {
        format!("{directory}{MAIN_SEPARATOR}{name}")
    }
}

fn is_denied(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::PermissionDenied
}

fn content_hash(path: &str) -> Option<u64> {
    let content = std::fs::read(path).ok()?;
    let mut hasher = DefaultHasher::new();
    hasher.write(&content);
    Some(hasher.finish())
}

struct Scanner<'a> {
    targets: &'a [ResolvedTarget],
    polling: bool,
    max_directories: usize,
    scan: Scan,
    counted: HashSet<String>,
    traversed: HashSet<(usize, String)>,
    /// Kinds of listed entries, from `lstat`.
    listed: HashMap<String, EntryKind>,
}

impl Scanner<'_> {
    fn count_directory(&mut self, path: &str) -> Result<(), ScanError> {
        self.counted.insert(path.to_string());
        if self.counted.len() > self.max_directories {
            return Err(ScanError::Budget(format!("Watched paths exceed {} directories", self.max_directories)));
        }
        Ok(())
    }

    fn record(&mut self, path: &str, metadata: &std::fs::Metadata) {
        let age = since_epoch_ns(Ok(SystemTime::now())).saturating_sub(since_epoch_ns(metadata.modified()));
        let recent = age < HASH_RECENT_MS * 1_000_000;
        let hashed = self.polling && metadata.is_file() && metadata.len() <= HASH_MAX_BYTES && recent;
        let hash = if hashed { content_hash(path) } else { None };
        self.scan.snapshot.insert(path.to_string(), entry_of(metadata, hash));
    }

    fn scan_directory(&mut self, index: usize, directory: &str) -> Result<(), ScanError> {
        if !self.traversed.insert((index, directory.to_string())) {
            return Ok(());
        }
        let target = &self.targets[index];
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            // The watched directory itself must be readable; below it, unreadable directories are skipped.
            Err(error) if directory == target.path && is_denied(&error) => return Err(error.into()),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied | io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        let names: Vec<String> =
            entries.flatten().map(|entry| entry.file_name().to_string_lossy().into_owned()).collect();
        for name in names {
            if self.targets[index].excluded(&name) {
                continue;
            }
            let path = child_path(directory, &name);
            let kind = match self.listed.get(&path) {
                Some(kind) => *kind,
                None => {
                    let Ok(metadata) = std::fs::symlink_metadata(&path) else { continue };
                    let kind = entry_of(&metadata, None).kind;
                    self.listed.insert(path.clone(), kind);
                    // A target's own entry (following links) wins over its listing by another target.
                    if !self.scan.snapshot.contains_key(&path) {
                        self.record(&path, &metadata);
                    }
                    kind
                }
            };
            if self.targets[index].recursive && kind == EntryKind::Directory {
                self.count_directory(&path)?;
                self.scan_directory(index, &path)?;
            }
        }
        Ok(())
    }

    fn scan_target(&mut self, index: usize) -> Result<(), ScanError> {
        let target_path = self.targets[index].path.clone();
        for ancestor in ancestors_of(&target_path) {
            if self.scan.snapshot.contains_key(&ancestor) {
                continue;
            }
            // Identity only: an ancestor's own timestamps change with every unrelated sibling.
            if let Ok(metadata) = std::fs::symlink_metadata(&ancestor) {
                let entry = Entry { size: 0, mtime_ns: 0, ..entry_of(&metadata, None) };
                self.scan.snapshot.insert(ancestor, entry);
            }
        }
        // The target may be a symbolic link to what is watched; follow it. A missing target is watched for its
        // creation; one that cannot be reached for lack of permission fails.
        let metadata = match std::fs::metadata(&target_path) {
            Ok(metadata) => metadata,
            Err(error) if is_denied(&error) => return Err(error.into()),
            Err(_) => return Ok(()),
        };
        self.record(&target_path, &metadata);
        let linked = std::fs::symlink_metadata(&target_path).is_ok_and(|link| link.file_type().is_symlink());
        if metadata.is_file() && linked {
            self.scan.linked_files.insert(target_path.clone());
        }
        if metadata.is_dir() {
            self.count_directory(&target_path)?;
            self.scan_directory(index, &target_path)?;
        }
        Ok(())
    }
}

/// Scan every target. Directories are counted once, but traversed once per target: overlapping targets differ in
/// recursion and exclusions, and an entry recorded for one target must still be descended into for another.
pub(crate) fn scan(targets: &[ResolvedTarget], polling: bool, max_directories: usize) -> Result<Scan, ScanError> {
    let mut scanner = Scanner {
        targets,
        polling,
        max_directories,
        scan: Scan::default(),
        counted: HashSet::new(),
        traversed: HashSet::new(),
        listed: HashMap::new(),
    };
    for index in 0..targets.len() {
        scanner.scan_target(index)?;
    }
    Ok(scanner.scan)
}

/// An ancestor that changed identity moved every target below it; report those targets.
pub(crate) fn reported(path: &str, targets: &[ResolvedTarget]) -> String {
    if targets.iter().any(|target| is_within(path, &target.path)) {
        return path.to_string();
    }
    targets
        .iter()
        .find(|target| is_within(&target.path, path))
        .map_or_else(|| path.to_string(), |target| target.path.clone())
}

/// Paths that differ between two snapshots, as reported.
pub(crate) fn diff(previous: &Snapshot, next: &Snapshot, targets: &[ResolvedTarget]) -> Vec<String> {
    let mut changed = Vec::new();
    for (path, entry) in next {
        if previous.get(path) != Some(entry) {
            changed.push(reported(path, targets));
        }
    }
    for path in previous.keys() {
        if !next.contains_key(path) {
            changed.push(reported(path, targets));
        }
    }
    changed
}

/// Whether an event at `path` concerns a target: an ancestor of one, or an entry below one that is not excluded.
pub(crate) fn in_scope(path: &str, targets: &[ResolvedTarget]) -> bool {
    targets.iter().any(|target| {
        if is_within(&target.path, path) {
            return true;
        }
        if !is_within(path, &target.path) || path == target.path {
            return false;
        }
        let relative = Path::new(path).strip_prefix(&target.path).unwrap_or(Path::new(path));
        let components: Vec<String> =
            relative.components().map(|component| component.as_os_str().to_string_lossy().into_owned()).collect();
        if !target.recursive && components.len() > 1 {
            return false;
        }
        !components.iter().any(|name| target.excluded(name))
    })
}

/// Linux `statfs` magic numbers of file systems that accept watches but do not report changes made elsewhere: NFS,
/// SMB, CIFS, SMB2, FUSE (sshfs), 9P (WSL2 drives), Lustre, GPFS, Ceph, OpenAFS, kAFS, and sdcardfs.
#[cfg(any(target_os = "linux", target_os = "android"))]
const UNRELIABLE_FILE_SYSTEMS: [u32; 12] = [
    0x6969,
    0x517b,
    0xff53_4d42,
    0xfe53_4d42,
    0x6573_5546,
    0x0102_1997,
    0x0bd0_0bd0,
    0x4750_4653,
    0x00c3_6400,
    0x5346_414f,
    0x6b41_4653,
    0x5dca_2df5,
];

/// Whether any path, or its nearest existing ancestor, is on a file system that does not report remote changes.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn any_unreliable(paths: &[String]) -> bool {
    paths.iter().any(|path| {
        let candidates = std::iter::once(path.clone()).chain(ancestors_of(path));
        for candidate in candidates {
            if let Ok(info) = rustix::fs::statfs(candidate.as_str()) {
                #[allow(clippy::unnecessary_cast)]
                let kind = info.f_type as u32;
                return UNRELIABLE_FILE_SYSTEMS.contains(&kind);
            }
        }
        false
    })
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(crate) fn any_unreliable(_paths: &[String]) -> bool {
    false
}
