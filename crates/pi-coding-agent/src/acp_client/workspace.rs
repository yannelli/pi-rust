//! Workspace snapshots around an ACP turn, for `/undo` of the changes the agent's own tools make
//! (shell commands included). pi compares file content, so same-size, same-time edits count.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::agent_session::MAX_CHECKPOINT_FILE_BYTES;
use crate::tools::{PriorContent, too_large_reason};

/// The most file content one snapshot keeps.
pub const MAX_SNAPSHOT_BYTES: u64 = 256 * 1024 * 1024;
const UNSEEN: &str = "pi did not see it before the turn (.gitignore excluded it, or pi could not list it)";

#[derive(Clone, Debug, PartialEq)]
enum Kept {
    Bytes(Vec<u8>),
    /// Only the size and time are known; the text says why.
    Metadata {
        len: u64,
        modified: Option<std::time::SystemTime>,
        reason: String,
    },
}

/// The files of the working directory at the start of a turn.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    files: BTreeMap<PathBuf, Kept>,
    taken: Option<SystemTime>,
}

/// One difference between a snapshot and the files now. pi keeps no content of the file now.
#[derive(Clone, Debug, PartialEq)]
pub struct Difference {
    pub path: PathBuf,
    pub prior: PriorContent,
    /// `false` when the file is gone.
    pub exists: bool,
}

/// The files under `root` that `.gitignore` does not exclude; hidden files count, `.git` does not.
fn files(root: &Path) -> Vec<(PathBuf, std::fs::Metadata)> {
    let walk = ignore::WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .follow_links(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build();
    walk.filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .filter_map(|entry| Some((entry.path().to_path_buf(), entry.metadata().ok()?)))
        .collect()
}

/// The content of `path` when it has at most `limit` bytes; the read stops after `limit + 1`.
fn read_bounded(path: &Path, limit: u64) -> std::io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    Ok((bytes.len() as u64 <= limit).then_some(bytes))
}

/// The files under `root`, with content up to the size limits.
pub fn take(root: &Path) -> Snapshot {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut budget = MAX_SNAPSHOT_BYTES;
    let mut snapshot = Snapshot { taken: Some(SystemTime::now()), ..Snapshot::default() };
    for (path, meta) in files(&root) {
        let len = meta.len();
        let metadata = |reason: String| Kept::Metadata { len, modified: meta.modified().ok(), reason };
        let total = || format!("the workspace snapshot keeps at most {} MiB", MAX_SNAPSHOT_BYTES >> 20);
        let kept = if len > MAX_CHECKPOINT_FILE_BYTES {
            metadata(too_large_reason(MAX_CHECKPOINT_FILE_BYTES))
        } else if len > budget {
            metadata(total())
        } else {
            match read_bounded(&path, MAX_CHECKPOINT_FILE_BYTES.min(budget)) {
                Ok(Some(bytes)) => {
                    budget -= bytes.len() as u64;
                    Kept::Bytes(bytes)
                }
                Ok(None) if budget < MAX_CHECKPOINT_FILE_BYTES => metadata(total()),
                Ok(None) => metadata(too_large_reason(MAX_CHECKPOINT_FILE_BYTES)),
                Err(error) => metadata(format!("pi could not read it: {error}")),
            }
        };
        snapshot.files.insert(path, kept);
    }
    snapshot
}

impl Snapshot {
    /// The state of `path` in the snapshot, when the snapshot covers it.
    pub fn prior(&self, path: &Path) -> Option<PriorContent> {
        Some(match self.files.get(path)? {
            Kept::Bytes(bytes) => PriorContent::Bytes(bytes.clone()),
            Kept::Metadata { reason, .. } => PriorContent::NotRestorable(reason.clone()),
        })
    }

    /// Whether the snapshot covers `path`: a file it saw, or a missing file under `root`.
    pub fn covers(&self, root: &Path, path: &Path) -> bool {
        self.files.contains_key(path) || (path.starts_with(root) && !path.exists())
    }

    /// What changed under `root` since the snapshot.
    pub fn differences(&self, root: &Path) -> Vec<Difference> {
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let now = files(&root);
        let present: BTreeSet<&PathBuf> = now.iter().map(|(path, _)| path).collect();
        let mut out = Vec::new();
        for (path, meta) in &now {
            let prior = match self.files.get(path) {
                None => match self.unseen_prior(meta) {
                    Some(prior) => prior,
                    None => continue,
                },
                Some(Kept::Bytes(before)) => {
                    let now = read_bounded(path, before.len() as u64).ok().flatten();
                    let same = meta.len() == before.len() as u64 && now.as_ref() == Some(before);
                    if same {
                        continue;
                    }
                    PriorContent::Bytes(before.clone())
                }
                Some(Kept::Metadata { len, modified, reason }) => {
                    if *len == meta.len() && *modified == meta.modified().ok() {
                        continue;
                    }
                    PriorContent::NotRestorable(reason.clone())
                }
            };
            out.push(Difference { path: path.clone(), prior, exists: true });
        }
        for path in self.files.keys().filter(|path| !present.contains(path) && !path.exists()) {
            let prior = self.prior(path).expect("the snapshot has the path");
            out.push(Difference { path: path.clone(), prior, exists: false });
        }
        out
    }

    /// A file the snapshot did not list is new when it was created after the snapshot. One that
    /// existed and changed during the turn is not restorable; one that did not change is skipped.
    fn unseen_prior(&self, meta: &std::fs::Metadata) -> Option<PriorContent> {
        let after = |time: std::io::Result<SystemTime>| match (time, self.taken) {
            (Ok(time), Some(taken)) => Some(time >= taken),
            _ => None,
        };
        match (after(meta.created()), after(meta.modified())) {
            (Some(true), _) => Some(PriorContent::Missing),
            (Some(false), Some(false)) => None,
            _ => Some(PriorContent::NotRestorable(UNSEEN.into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(before: &Snapshot, root: &Path) -> Vec<(String, PriorContent, bool)> {
        let mut found = before.differences(root);
        found.sort_by(|a, b| a.path.cmp(&b.path));
        let name = |path: &Path| path.file_name().unwrap().to_string_lossy().into_owned();
        found.into_iter().map(|change| (name(&change.path), change.prior, change.exists)).collect()
    }

    #[test]
    fn content_changes_new_files_and_deletes_are_found() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for (name, text) in [("same.txt", "abc"), ("gone.txt", "bye"), ("kept.txt", "keep"), ("skip.log", "x")] {
            std::fs::write(root.join(name), text).unwrap();
        }
        std::fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        let before = take(&root);
        let modified = std::fs::metadata(root.join("same.txt")).unwrap().modified().unwrap();
        std::fs::write(root.join("same.txt"), "xyz").unwrap();
        std::fs::File::options().write(true).open(root.join("same.txt")).unwrap().set_modified(modified).unwrap();
        std::fs::remove_file(root.join("gone.txt")).unwrap();
        std::fs::write(root.join("new.txt"), "hi").unwrap();
        std::fs::write(root.join("skip.log"), "changed").unwrap();
        assert_eq!(
            summary(&before, &root),
            vec![
                ("gone.txt".into(), PriorContent::Bytes(b"bye".to_vec()), false),
                ("new.txt".into(), PriorContent::Missing, true),
                ("same.txt".into(), PriorContent::Bytes(b"abc".to_vec()), true),
            ]
        );
    }

    #[test]
    fn files_that_an_ignore_rule_hid_are_not_new() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join(".gitignore"), "secret.txt\nquiet.txt\n").unwrap();
        std::fs::write(root.join("secret.txt"), "old secret").unwrap();
        std::fs::write(root.join("quiet.txt"), "untouched").unwrap();
        let before = take(&root);
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(root.join(".gitignore"), "").unwrap();
        std::fs::write(root.join("secret.txt"), "new secret").unwrap();
        let found = summary(&before, &root);
        let secret = found.iter().find(|(name, _, _)| name == "secret.txt").expect("the change is listed");
        assert!(matches!(&secret.1, PriorContent::NotRestorable(reason) if reason == UNSEEN), "{secret:?}");
        assert!(found.iter().all(|(name, _, _)| name != "quiet.txt"), "an unchanged hidden file is skipped");
    }

    #[test]
    fn reads_stop_at_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grown.bin");
        std::fs::write(&path, b"12345").unwrap();
        assert_eq!(read_bounded(&path, 5).unwrap(), Some(b"12345".to_vec()));
        assert_eq!(read_bounded(&path, 4).unwrap(), None, "a file that grew past the limit is not kept");
    }

    #[test]
    fn large_files_are_compared_by_size_and_time() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let big = vec![b'x'; MAX_CHECKPOINT_FILE_BYTES as usize + 1];
        std::fs::write(root.join("big.bin"), &big).unwrap();
        let before = take(&root);
        assert!(matches!(before.prior(&root.join("big.bin")), Some(PriorContent::NotRestorable(_))));
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(root.join("big.bin"), [&big[..], b"y"].concat()).unwrap();
        std::fs::write(root.join("output.bin"), &big).unwrap();
        let found = summary(&before, &root);
        assert!(matches!(&found[0], (name, PriorContent::NotRestorable(_), true) if name == "big.bin"), "{found:?}");
        assert_eq!(found[1], ("output.bin".into(), PriorContent::Missing, true), "a large new file can be undone");
    }
}
