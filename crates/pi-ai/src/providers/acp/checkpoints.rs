//! File changes that the agent names in a turn (`fs/write_text_file`, and `edit`, `delete`, or
//! `move` tool calls), for the `/undo` checkpoints of the host.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::client::inside;
use super::host::{FileChange, PriorFile, host};

/// Files larger than this before a change are not kept.
pub(crate) const MAX_SNAPSHOT_BYTES: u64 = 4 * 1024 * 1024;
const CHANGING_KINDS: [&str; 3] = ["edit", "delete", "move"];

#[derive(Default)]
struct ToolFiles {
    kind: String,
    paths: Vec<PathBuf>,
    finished: bool,
}

/// What one turn changed. The first state of each path is kept until the turn ends.
#[derive(Default)]
pub(crate) struct Checkpoints {
    priors: HashMap<PathBuf, PriorFile>,
    tools: HashMap<String, ToolFiles>,
}

pub(crate) fn snapshot(path: &Path) -> PriorFile {
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() > MAX_SNAPSHOT_BYTES => {
            PriorFile::NotRestorable(format!("it was larger than {} MiB", MAX_SNAPSHOT_BYTES >> 20))
        }
        Ok(_) => std::fs::read(path).map_or_else(|error| PriorFile::NotRestorable(error.to_string()), PriorFile::Bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => PriorFile::Missing,
        Err(error) => PriorFile::NotRestorable(error.to_string()),
    }
}

fn unchanged(prior: &PriorFile, written: &Option<Vec<u8>>) -> bool {
    match (prior, written) {
        (PriorFile::Bytes(before), Some(after)) => before == after,
        (PriorFile::Missing, None) => true,
        _ => false,
    }
}

/// Reports `path` when it differs from `prior`. A file now larger than the limit is left to the
/// workspace scan of the host, which does not read it whole.
fn report(pi_session: &str, path: &Path, prior: &PriorFile) {
    if std::fs::metadata(path).is_ok_and(|meta| meta.len() > MAX_SNAPSHOT_BYTES) {
        return;
    }
    let written = std::fs::read(path).ok();
    if unchanged(prior, &written) {
        return;
    }
    if let Some(host) = host() {
        let change = FileChange { path: path.to_path_buf(), prior: prior.clone(), written };
        host.file_changed(pi_session, change);
    }
}

impl Checkpoints {
    /// Starts a turn.
    pub fn clear(&mut self) {
        self.priors.clear();
        self.tools.clear();
    }

    /// Keeps the state of `path` before its first change of the turn.
    pub fn before_write(&mut self, path: &Path) {
        self.priors.entry(path.to_path_buf()).or_insert_with(|| snapshot(path));
    }

    /// Reports `path` after `fs/write_text_file`.
    pub fn after_write(&self, pi_session: &str, path: &Path) {
        if let Some(prior) = self.priors.get(path) {
            report(pi_session, path, prior);
        }
    }

    /// Keeps the files that a changing tool names before it runs and reports them when it ends.
    /// The workspace scan of the host covers files first named after the change.
    pub fn tool_update(&mut self, pi_session: &str, cwd: &Path, update: &Value) {
        let id = update["toolCallId"].as_str().unwrap_or_default().to_string();
        let tool = self.tools.entry(id).or_default();
        if let Some(kind) = update["kind"].as_str() {
            tool.kind = kind.to_string();
        }
        let content = update["content"].as_array().into_iter().flatten();
        let diffs: Vec<&Value> = content.filter(|item| item["type"] == "diff").collect();
        let named = update["locations"].as_array().into_iter().flatten().chain(diffs.iter().copied());
        for path in named.filter_map(|item| inside(cwd, item["path"].as_str()?).ok()) {
            if !tool.paths.contains(&path) {
                tool.paths.push(path);
            }
        }
        if !CHANGING_KINDS.contains(&tool.kind.as_str()) && diffs.is_empty() {
            return;
        }
        if !matches!(update["status"].as_str(), Some("completed" | "failed")) {
            for path in &tool.paths {
                self.priors.entry(path.clone()).or_insert_with(|| snapshot(path));
            }
            return;
        }
        if std::mem::replace(&mut tool.finished, true) {
            return;
        }
        for path in &tool.paths {
            if let Some(prior) = self.priors.get(path) {
                report(pi_session, path, prior);
            }
        }
    }
}
