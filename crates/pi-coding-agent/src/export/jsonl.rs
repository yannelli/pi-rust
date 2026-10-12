//! Branch JSONL export (upstream `core/session-export.ts`): a fresh header, then the current
//! branch with `parentId` relinked into a chain, then optional export-only entries.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::json::stringify;
use super::session_file::{CURRENT_SESSION_VERSION, SessionSnapshot, iso_timestamp};

/// Builds trailing entries from the last branch entry id and the export timestamp.
pub type TrailingEntries<'a> = &'a dyn Fn(Option<&str>, &str) -> Vec<Value>;

/// Serialize the current branch of the session as JSONL, ending with a newline.
pub fn serialize_session_branch(snapshot: &SessionSnapshot, trailing: Option<TrailingEntries<'_>>) -> String {
    serialize_at(snapshot, trailing, &iso_timestamp(pi_ai::now_ms()))
}

pub(super) fn serialize_at(
    snapshot: &SessionSnapshot,
    trailing: Option<TrailingEntries<'_>>,
    timestamp: &str,
) -> String {
    let mut header = Map::new();
    header.insert("type".into(), "session".into());
    header.insert("version".into(), CURRENT_SESSION_VERSION.into());
    header.insert("id".into(), snapshot.session_id().into());
    header.insert("timestamp".into(), timestamp.into());
    header.insert("cwd".into(), snapshot.cwd().display().to_string().into());

    let mut lines = vec![stringify(&Value::Object(header))];
    let mut parent_id: Option<String> = None;
    for entry in snapshot.branch() {
        let mut entry = entry.clone();
        if let Some(object) = entry.as_object_mut() {
            object.insert("parentId".into(), parent_id.clone().map_or(Value::Null, Value::String));
        }
        parent_id = entry.get("id").and_then(Value::as_str).map(str::to_string);
        lines.push(stringify(&entry));
    }
    if let Some(trailing) = trailing {
        lines.extend(trailing(parent_id.as_deref(), timestamp).iter().map(stringify));
    }
    format!("{}\n", lines.join("\n"))
}

/// `session-<ISO timestamp with : and . replaced by ->.jsonl`.
pub fn default_jsonl_output_path() -> PathBuf {
    PathBuf::from(format!("session-{}.jsonl", iso_timestamp(pi_ai::now_ms()).replace([':', '.'], "-")))
}

/// Write the current branch of `session_file` as JSONL. Relative `output_path` values
/// resolve against `cwd`; parent directories are created. Returns the absolute path.
pub fn export_session_to_jsonl(
    session_file: &Path,
    output_path: Option<&str>,
    cwd: &Path,
    trailing: Option<TrailingEntries<'_>>,
) -> anyhow::Result<PathBuf> {
    let snapshot = SessionSnapshot::load(session_file)?;
    let target = match output_path {
        Some(path) => crate::config::expand_tilde(path),
        None => default_jsonl_output_path(),
    };
    let file_path = if target.is_absolute() { target } else { cwd.join(target) };
    if let Some(dir) = file_path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&file_path, serialize_session_branch(&snapshot, trailing))?;
    Ok(file_path)
}
