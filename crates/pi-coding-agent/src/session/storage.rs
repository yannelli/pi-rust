//! Reading session files: JSONL parsing, header discovery, and v1/v2 migration.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::entry::{CURRENT_SESSION_VERSION, FileEntry, SessionHeader};
use super::ids::{generate_entry_id, resolve_path};

/// Header discovery reads at most this much before giving up on a file.
pub const MAX_SESSION_HEADER_SCAN_BYTES: u64 = 1024 * 1024;

fn parse_line(line: &[u8]) -> Option<FileEntry> {
    let text = String::from_utf8_lossy(line);
    if text.trim().is_empty() {
        return None;
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(object)) => Some(object),
        _ => None,
    }
}

/// Parse JSONL text, skipping blank and malformed lines.
pub fn parse_session_entries(content: &str) -> Vec<FileEntry> {
    content.split('\n').filter_map(|line| parse_line(line.as_bytes())).collect()
}

fn is_header(entry: &FileEntry) -> bool {
    entry.get("type").and_then(Value::as_str) == Some("session") && entry.get("id").is_some_and(Value::is_string)
}

/// The typed header of a raw `session` line, tolerating missing or odd fields.
pub fn header_from_raw(raw: &FileEntry) -> Option<SessionHeader> {
    if !is_header(raw) {
        return None;
    }
    let text = |key: &str| raw.get(key).and_then(Value::as_str).map(String::from);
    Some(SessionHeader {
        kind: "session".into(),
        version: raw.get("version").and_then(Value::as_u64).map(|version| version as u32),
        id: text("id").unwrap_or_default(),
        timestamp: text("timestamp").unwrap_or_default(),
        cwd: text("cwd").unwrap_or_default(),
        parent_session: text("parentSession"),
    })
}

/// All lines of a session file. Empty when the file is missing or its first parsed line is not
/// a session header. A valid file missing its final newline gets one appended.
pub fn load_entries_from_file(path: &Path) -> Vec<FileEntry> {
    let Ok(bytes) = std::fs::read(path) else { return Vec::new() };
    let mut lines: Vec<&[u8]> = bytes.split(|byte| *byte == b'\n').collect();
    let pending = lines.pop().unwrap_or_default();
    let mut entries: Vec<FileEntry> = lines.into_iter().filter_map(parse_line).collect();
    entries.extend(parse_line(pending));
    if entries.first().is_none_or(|first| !is_header(first)) {
        return Vec::new();
    }
    if !pending.is_empty()
        && let Ok(mut file) = std::fs::OpenOptions::new().append(true).open(path)
    {
        let _ = file.write_all(b"\n");
    }
    entries
}

/// Error for headers beyond [`MAX_SESSION_HEADER_SCAN_BYTES`].
#[derive(Debug)]
pub struct HeaderScanLimit(pub PathBuf);

impl std::fmt::Display for HeaderScanLimit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Session header exceeds {MAX_SESSION_HEADER_SCAN_BYTES}-byte scan limit: {}",
            self.0.display()
        )
    }
}

impl std::error::Error for HeaderScanLimit {}

/// The header from the first parsed line, reading at most the scan limit.
pub fn read_session_header(path: &Path) -> anyhow::Result<Option<SessionHeader>> {
    let file = std::fs::File::open(path)?;
    let mut buffer = Vec::new();
    file.take(MAX_SESSION_HEADER_SCAN_BYTES + 1).read_to_end(&mut buffer)?;
    let complete = buffer.len() as u64 <= MAX_SESSION_HEADER_SCAN_BYTES;
    let mut lines: Vec<&[u8]> = buffer.split(|byte| *byte == b'\n').collect();
    let pending = lines.pop().unwrap_or_default();
    for line in lines {
        if let Some(entry) = parse_line(line) {
            return Ok(header_from_raw(&entry));
        }
    }
    if !complete {
        return Err(HeaderScanLimit(path.to_path_buf()).into());
    }
    Ok(parse_line(pending).and_then(|entry| header_from_raw(&entry)))
}

/// Best-effort header discovery: unreadable or oversized files are not sessions.
pub fn read_session_header_for_discovery(path: &Path) -> Option<SessionHeader> {
    read_session_header(path).ok().flatten()
}

pub(crate) fn cwd_matches(cwd: &str, resolved: &Path) -> bool {
    !cwd.is_empty() && resolve_path(Path::new(cwd)) == resolved
}

/// The session file in `dir` with the latest change time, optionally only for `cwd`.
pub fn find_most_recent_session(dir: &Path, cwd: Option<&Path>) -> Option<PathBuf> {
    let resolved_cwd = cwd.map(resolve_path);
    let mut files: Vec<(PathBuf, std::time::SystemTime)> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "jsonl"))
        .filter_map(|path| {
            let changed = std::fs::metadata(&path).and_then(|metadata| metadata.modified()).ok()?;
            Some((path, changed))
        })
        .collect();
    files.sort_by_key(|file| std::cmp::Reverse(file.1));
    files.into_iter().map(|(path, _)| path).find(|path| {
        read_session_header_for_discovery(path)
            .is_some_and(|header| resolved_cwd.as_deref().is_none_or(|cwd| cwd_matches(&header.cwd, cwd)))
    })
}

fn migrate_v1_to_v2(entries: &mut [FileEntry]) {
    let mut ids: HashSet<String> = HashSet::new();
    let mut previous: Option<String> = None;
    for index in 0..entries.len() {
        if entries[index].get("type").and_then(Value::as_str) == Some("session") {
            entries[index].insert("version".into(), Value::from(2));
            continue;
        }
        let id = generate_entry_id(|candidate| ids.contains(candidate));
        ids.insert(id.clone());
        entries[index].insert("id".into(), Value::String(id.clone()));
        entries[index].insert("parentId".into(), previous.map_or(Value::Null, Value::String));
        previous = Some(id);
        if entries[index].get("type").and_then(Value::as_str) == Some("compaction")
            && let Some(kept_index) = entries[index].get("firstKeptEntryIndex").cloned()
        {
            if let Some(target) = kept_index.as_u64().and_then(|kept| entries.get(kept as usize))
                && target.get("type").and_then(Value::as_str) != Some("session")
                && let Some(target_id) = target.get("id").cloned()
            {
                entries[index].insert("firstKeptEntryId".into(), target_id);
            }
            if kept_index.is_number() {
                entries[index].shift_remove("firstKeptEntryIndex");
            }
        }
    }
}

fn migrate_v2_to_v3(entries: &mut [FileEntry]) {
    for entry in entries.iter_mut() {
        match entry.get("type").and_then(Value::as_str) {
            Some("session") => {
                entry.insert("version".into(), Value::from(3));
            }
            Some("message") => {
                if let Some(message) = entry.get_mut("message").and_then(Value::as_object_mut)
                    && message.get("role").and_then(Value::as_str) == Some("hookMessage")
                {
                    message.insert("role".into(), Value::String("custom".into()));
                }
            }
            _ => {}
        }
    }
}

/// Bring entries to the current version in place. Returns whether anything changed.
pub fn migrate_session_entries(entries: &mut [FileEntry]) -> bool {
    let header = entries.iter().find(|entry| entry.get("type").and_then(Value::as_str) == Some("session"));
    let version = header.and_then(|header| header.get("version")).and_then(Value::as_u64).unwrap_or(1);
    if version >= u64::from(CURRENT_SESSION_VERSION) {
        return false;
    }
    if version < 2 {
        migrate_v1_to_v2(entries);
    }
    migrate_v2_to_v3(entries);
    true
}

/// Write `lines` as JSONL, replacing the file (or failing if `create_new` and it exists).
pub(crate) fn write_lines(path: &Path, lines: &[String], create_new: bool) -> anyhow::Result<()> {
    if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    if create_new {
        options.create_new(true);
    } else {
        options.create(true).truncate(true);
    }
    let mut file = options.open(path)?;
    let mut text = String::new();
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    file.write_all(text.as_bytes())?;
    Ok(())
}

pub(crate) fn append_line(path: &Path, line: &str) -> anyhow::Result<()> {
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(format!("{line}\n").as_bytes())?;
    Ok(())
}
