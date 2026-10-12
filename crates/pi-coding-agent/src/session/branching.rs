//! Copying sessions: fork into another project, export the current branch as JSONL, import
//! a JSONL file, and the missing-cwd check for resumed or imported sessions.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use super::entry::{CURRENT_SESSION_VERSION, SessionHeader};
use super::ids::{assert_valid_session_id, iso_timestamp, resolve_path, uuid_v7};
use super::storage::{load_entries_from_file, migrate_session_entries, write_lines};
use super::{NewSessionOptions, SessionManager, session_dir};

/// Builds export-only entries from `(parent id, timestamp)`.
pub type TrailingEntries = dyn Fn(Option<&str>, &str) -> Vec<Map<String, Value>>;

impl SessionManager {
    /// Copy the session at `source` into a new file for `target_cwd`, with `parentSession`
    /// pointing at the source. `dir` defaults to the target's session directory.
    pub fn fork_from(
        source: &Path,
        target_cwd: &Path,
        dir: Option<&Path>,
        agent_dir: &Path,
        options: NewSessionOptions,
    ) -> anyhow::Result<Self> {
        let source = resolve_path(source);
        let target_cwd = resolve_path(target_cwd);
        let mut entries = load_entries_from_file(&source);
        migrate_session_entries(&mut entries);
        if entries.is_empty() {
            anyhow::bail!("Cannot fork: source session file is empty or invalid: {}", source.display());
        }
        let dir = dir.map(resolve_path).unwrap_or_else(|| session_dir(agent_dir, &target_cwd));
        std::fs::create_dir_all(&dir)?;
        if let Some(id) = &options.id {
            assert_valid_session_id(id)?;
        }
        let id = options.id.unwrap_or_else(uuid_v7);
        let timestamp = iso_timestamp();
        let file = dir.join(format!("{}_{id}.jsonl", timestamp.replace([':', '.'], "-")));
        let header = SessionHeader {
            kind: "session".into(),
            version: Some(CURRENT_SESSION_VERSION),
            id,
            timestamp,
            cwd: target_cwd.display().to_string(),
            parent_session: Some(source.display().to_string()),
        };
        let mut lines = vec![serde_json::to_string(&header)?];
        lines.extend(
            entries
                .iter()
                .filter(|entry| entry.get("type").and_then(Value::as_str) != Some("session"))
                .map(|entry| serde_json::to_string(entry).unwrap_or_default()),
        );
        write_lines(&file, &lines, true)?;
        Self::open_with(&file, Some(&dir), Some(&target_cwd))
    }

    /// The current branch as JSONL: a fresh header (same id), then the path entries re-chained
    /// from the root, then `trailing` export-only entries built from `(parent id, timestamp)`.
    pub fn serialize_branch(&self, trailing: Option<&TrailingEntries>) -> String {
        let timestamp = iso_timestamp();
        let header = json!({
            "type": "session",
            "version": CURRENT_SESSION_VERSION,
            "id": self.session_id(),
            "timestamp": timestamp,
            "cwd": self.cwd().display().to_string(),
        });
        let mut lines = vec![header.to_string()];
        let mut parent: Option<String> = None;
        for entry in self.get_branch(None) {
            lines.push(entry.with_parent(parent.as_deref()).to_json_line());
            parent = Some(entry.id().to_string());
        }
        if let Some(trailing) = trailing {
            for entry in trailing(parent.as_deref(), &timestamp) {
                lines.push(serde_json::to_string(&entry).unwrap_or_default());
            }
        }
        let mut text = lines.join("\n");
        text.push('\n');
        text
    }

    /// Write [`Self::serialize_branch`] to `output` (default `session-<timestamp>.jsonl` in the
    /// process cwd). Returns the written path.
    pub fn export_branch_to_jsonl(&self, output: Option<&Path>) -> anyhow::Result<PathBuf> {
        let default_name = format!("session-{}.jsonl", iso_timestamp().replace([':', '.'], "-"));
        let path = resolve_path(output.unwrap_or(Path::new(&default_name)));
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, self.serialize_branch(None))?;
        Ok(path)
    }
}

/// Copy a JSONL session into `session_dir` (adding `-1`, `-2`, ... on name clashes, or
/// reusing it when already stored there) and open it, optionally with another cwd.
pub fn import_session_file(input: &Path, dir: &Path, cwd_override: Option<&Path>) -> anyhow::Result<SessionManager> {
    let input = resolve_path(input);
    if !input.exists() {
        anyhow::bail!("Session file not found: {}", input.display());
    }
    let dir = resolve_path(dir);
    std::fs::create_dir_all(&dir)?;
    let file_name = input.file_name().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("session.jsonl"));
    let mut destination = dir.join(&file_name);
    if destination != input {
        let stem = file_name.file_stem().map(|stem| stem.to_string_lossy().to_string()).unwrap_or_default();
        let extension = file_name.extension().map(|ext| format!(".{}", ext.to_string_lossy())).unwrap_or_default();
        let mut suffix = 1;
        while destination.exists() {
            destination = dir.join(format!("{stem}-{suffix}{extension}"));
            suffix += 1;
        }
        std::fs::copy(&input, &destination)?;
    }
    SessionManager::open_with(&destination, Some(&dir), cwd_override)
}

/// A stored session cwd that no longer exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissingSessionCwd {
    pub session_file: Option<PathBuf>,
    pub session_cwd: PathBuf,
    pub fallback_cwd: PathBuf,
}

impl MissingSessionCwd {
    /// The error text for non-interactive modes.
    pub fn error_message(&self) -> String {
        let file = self.session_file.as_ref().map(|file| format!("\nSession file: {}", file.display()));
        format!(
            "Stored session working directory does not exist: {}{}\nCurrent working directory: {}",
            self.session_cwd.display(),
            file.unwrap_or_default(),
            self.fallback_cwd.display()
        )
    }

    /// The prompt text for interactive confirmation.
    pub fn prompt(&self) -> String {
        format!(
            "cwd from session file does not exist\n{}\n\ncontinue in current cwd\n{}",
            self.session_cwd.display(),
            self.fallback_cwd.display()
        )
    }
}

/// The missing-cwd problem of a file-backed session, if its cwd is gone.
pub fn missing_session_cwd(session: &SessionManager, fallback_cwd: &Path) -> Option<MissingSessionCwd> {
    let file = session.session_file()?;
    let cwd = session.cwd();
    if cwd.as_os_str().is_empty() || cwd.exists() {
        return None;
    }
    Some(MissingSessionCwd {
        session_file: Some(file.to_path_buf()),
        session_cwd: cwd.to_path_buf(),
        fallback_cwd: fallback_cwd.to_path_buf(),
    })
}
