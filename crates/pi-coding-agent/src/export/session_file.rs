//! Read-only view of a session JSONL file, as upstream `SessionManager.open` builds it:
//! malformed lines are skipped, old versions are migrated in memory, and the leaf is the
//! last entry. The file itself is never written.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, Hasher};
use std::path::{Path, PathBuf};

use anyhow::bail;
use serde_json::{Map, Value};

use super::html::APP_NAME;

pub const CURRENT_SESSION_VERSION: u64 = 3;

#[derive(Clone, Debug, PartialEq)]
pub struct SessionSnapshot {
    pub path: PathBuf,
    /// The `session` header object, with its fields in file order.
    pub header: Map<String, Value>,
    /// Every non-header entry in file order, across all branches.
    pub entries: Vec<Value>,
    /// The last entry's `id` value: `Some(Null)` without entries, `None` when that entry has no id.
    pub leaf_id: Option<Value>,
}

impl SessionSnapshot {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let bytes = std::fs::read(path)?;
        let file_entries = parse_session_entries(&String::from_utf8_lossy(&bytes));
        if file_entries.is_empty() && !bytes.is_empty() {
            bail!("Session file is not a valid {APP_NAME} session: {}", path.display());
        }
        Ok(Self::from_values(path, file_entries))
    }

    /// Build from parsed file entries (header included), for sessions held in memory.
    pub fn from_values(path: impl Into<PathBuf>, mut file_entries: Vec<Value>) -> Self {
        let has_header = file_entries.iter().any(|entry| entry_type(entry) == Some("session"));
        if has_header {
            migrate_to_current_version(&mut file_entries);
        } else {
            file_entries.insert(0, Value::Object(new_header()));
        }
        let header = file_entries
            .iter()
            .find(|entry| entry_type(entry) == Some("session"))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let entries: Vec<Value> =
            file_entries.into_iter().filter(|entry| entry_type(entry) != Some("session")).collect();
        let leaf_id = match entries.last() {
            None => Some(Value::Null),
            Some(entry) => entry.get("id").cloned(),
        };
        Self { path: path.into(), header, entries, leaf_id }
    }

    /// The entries and current leaf of a live session, which may differ from its file's last entry.
    pub fn from_manager(manager: &crate::session::SessionManager) -> Self {
        Self {
            path: manager.session_file().map(Path::to_path_buf).unwrap_or_default(),
            header: manager.header_raw().clone(),
            entries: manager.entries().iter().map(|entry| Value::Object(entry.raw().clone())).collect(),
            leaf_id: Some(manager.leaf_id().map_or(Value::Null, Value::from)),
        }
    }

    pub fn session_id(&self) -> &str {
        self.header.get("id").and_then(Value::as_str).unwrap_or_default()
    }

    /// Header `cwd`, resolved against the process directory like upstream `resolvePath`.
    pub fn cwd(&self) -> PathBuf {
        let current = std::env::current_dir().unwrap_or_default();
        match self.header.get("cwd").and_then(Value::as_str) {
            Some(cwd) => current.join(crate::config::expand_tilde(cwd)),
            None => current,
        }
    }

    pub fn leaf_id_str(&self) -> Option<&str> {
        self.leaf_id.as_ref().and_then(Value::as_str)
    }

    /// Entries from the root to the leaf, following `parentId` links.
    pub fn branch(&self) -> Vec<&Value> {
        let by_id: HashMap<&str, &Value> =
            self.entries.iter().filter_map(|entry| Some((entry.get("id")?.as_str()?, entry))).collect();
        let mut path = Vec::new();
        let mut current = self.leaf_id_str().and_then(|id| by_id.get(id).copied());
        while let Some(entry) = current {
            if path.len() > self.entries.len() {
                break;
            }
            path.push(entry);
            current = entry.get("parentId").and_then(Value::as_str).and_then(|id| by_id.get(id).copied());
        }
        path.reverse();
        path
    }
}

/// Parse JSONL into objects, skipping blank and malformed lines (upstream `parseSessionEntries`).
pub fn parse_session_entries(content: &str) -> Vec<Value> {
    content
        .split('\n')
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(Value::is_object)
        .collect()
}

fn entry_type(entry: &Value) -> Option<&str> {
    entry.get("type").and_then(Value::as_str)
}

/// Upstream `migrateToCurrentVersion`: v1 adds the id tree, v2 renames `hookMessage`.
pub fn migrate_to_current_version(entries: &mut [Value]) {
    let version = entries
        .iter()
        .find(|entry| entry_type(entry) == Some("session"))
        .and_then(|header| header.get("version"))
        .and_then(Value::as_f64)
        .unwrap_or(1.0);
    if version >= CURRENT_SESSION_VERSION as f64 {
        return;
    }
    if version < 2.0 {
        migrate_v1_to_v2(entries);
    }
    if version < 3.0 {
        migrate_v2_to_v3(entries);
    }
}

fn migrate_v1_to_v2(entries: &mut [Value]) {
    let mut used = HashSet::new();
    let mut prev_id: Option<String> = None;
    for index in 0..entries.len() {
        if entry_type(&entries[index]) == Some("session") {
            entries[index]["version"] = Value::from(2);
            continue;
        }
        let id = loop {
            let id = random_hex(8);
            if used.insert(id.clone()) {
                break id;
            }
        };
        let kept_index = entries[index].get("firstKeptEntryIndex").and_then(Value::as_f64);
        let is_compaction = entry_type(&entries[index]) == Some("compaction");
        let Some(object) = entries[index].as_object_mut() else { continue };
        object.insert("id".into(), Value::String(id.clone()));
        object.insert("parentId".into(), prev_id.map_or(Value::Null, Value::String));
        prev_id = Some(id);
        let Some(kept_index) = kept_index.filter(|_| is_compaction) else { continue };
        let target = (kept_index >= 0.0 && kept_index.fract() == 0.0)
            .then(|| entries.get(kept_index as usize))
            .flatten()
            .filter(|target| entry_type(target) != Some("session"))
            .and_then(|target| target.get("id").cloned());
        let object = entries[index].as_object_mut().expect("checked above");
        if let Some(target_id) = target {
            object.insert("firstKeptEntryId".into(), target_id);
        }
        object.shift_remove("firstKeptEntryIndex");
    }
}

fn migrate_v2_to_v3(entries: &mut [Value]) {
    for entry in entries.iter_mut() {
        if entry_type(entry) == Some("session") {
            entry["version"] = Value::from(3);
            continue;
        }
        if entry_type(entry) == Some("message")
            && entry.pointer("/message/role").and_then(Value::as_str) == Some("hookMessage")
        {
            entry["message"]["role"] = Value::from("custom");
        }
    }
}

fn new_header() -> Map<String, Value> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let mut header = Map::new();
    header.insert("type".into(), "session".into());
    header.insert("version".into(), CURRENT_SESSION_VERSION.into());
    header.insert("id".into(), uuid_v7().into());
    header.insert("timestamp".into(), iso_timestamp(pi_ai::now_ms()).into());
    header.insert("cwd".into(), cwd.display().to_string().into());
    header
}

fn random_u64() -> u64 {
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
    hasher.write_u128(nanos);
    hasher.finish()
}

/// Lowercase hex string of `len` random digits.
pub fn random_hex(len: usize) -> String {
    let mut out = String::with_capacity(len);
    while out.len() < len {
        out.push_str(&format!("{:016x}", random_u64()));
    }
    out.truncate(len);
    out
}

fn uuid_v7() -> String {
    let millis = pi_ai::now_ms() as u64 & 0xffff_ffff_ffff;
    let (high, low) = (random_u64(), random_u64());
    let version = 0x7000 | (high & 0x0fff);
    let variant = 0x8000 | ((high >> 16) & 0x3fff);
    format!("{:08x}-{:04x}-{version:04x}-{variant:04x}-{:012x}", millis >> 16, millis & 0xffff, low & 0xffff_ffff_ffff)
}

/// `new Date(millis).toISOString()`: `YYYY-MM-DDTHH:MM:SS.mmmZ` in UTC.
pub fn iso_timestamp(millis: i64) -> String {
    let seconds = millis.div_euclid(1000);
    let (days, second_of_day) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        second_of_day / 3600,
        second_of_day % 3600 / 60,
        second_of_day % 60,
        millis.rem_euclid(1000)
    )
}
