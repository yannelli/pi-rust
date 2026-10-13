//! Crash records in `crashes.json` for the next start's notice and `/bug` (upstream
//! `core/crash-log.ts`), and a panic hook that writes them.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::platform::http::VERSION;

const MAX_CRASH_RECORDS: usize = 5;
const MAX_AGE_MS: i64 = 7 * 24 * 60 * 60 * 1000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CrashRecord {
    pub timestamp: String,
    pub version: String,
    /// `uncaught_exception` or `fatal_error`.
    pub kind: String,
    pub message: String,
    pub stack: Option<String>,
    pub session_file: Option<String>,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notified: Option<bool>,
}

/// `<agent dir>/crashes.json`.
pub fn crash_log_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("crashes.json")
}

/// Valid records; a missing or invalid file reads as empty.
pub fn read_crash_log(path: &Path) -> Vec<CrashRecord> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&text) else { return Vec::new() };
    items
        .into_iter()
        .filter(|item| {
            item.get("timestamp").is_some_and(Value::is_string) && item.get("message").is_some_and(Value::is_string)
        })
        .filter_map(|mut item| {
            let defaults = [("version", ""), ("kind", "fatal_error"), ("cwd", "")];
            for (key, default) in defaults {
                if !item.get(key).is_some_and(Value::is_string) {
                    item[key] = Value::String(default.into());
                }
            }
            serde_json::from_value(item).ok()
        })
        .collect()
}

fn write_crash_log(records: &[CrashRecord], path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(records)?))
}

/// What crashed, for [`record_crash`].
#[derive(Clone, Debug, Default)]
pub struct Crash {
    pub kind: String,
    pub message: String,
    pub stack: Option<String>,
    pub session_file: Option<String>,
    pub cwd: String,
}

/// Append a record, keeping the newest five. Best effort: callers are already failing.
pub fn record_crash(crash: Crash, path: &Path) -> Option<CrashRecord> {
    let record = CrashRecord {
        timestamp: crate::session::iso_timestamp(),
        version: VERSION.into(),
        kind: crash.kind,
        message: crash.message,
        stack: crash.stack,
        session_file: crash.session_file,
        cwd: crash.cwd,
        notified: None,
    };
    let mut records = read_crash_log(path);
    records.push(record.clone());
    let keep = records.len().saturating_sub(MAX_CRASH_RECORDS);
    write_crash_log(&records[keep..], path).ok()?;
    Some(record)
}

/// The newest unannounced crash from the last seven days; all records become announced.
pub fn take_unnotified_crash(path: &Path, now_ms: i64) -> Option<CrashRecord> {
    let records = read_crash_log(path);
    let recent = |record: &&CrashRecord| {
        crate::session::parse_iso(&record.timestamp).is_some_and(|time| now_ms - time <= MAX_AGE_MS)
    };
    let crash = records.iter().rev().filter(|record| record.notified != Some(true)).find(recent).cloned()?;
    let marked: Vec<CrashRecord> =
        records.into_iter().map(|record| CrashRecord { notified: Some(true), ..record }).collect();
    let _ = write_crash_log(&marked, path);
    Some(crash)
}

pub fn clear_crash_log(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Session file and working directory a crash should name; updated as sessions change.
#[derive(Clone, Default)]
pub struct CrashContext {
    inner: Arc<Mutex<(Option<String>, String)>>,
}

impl CrashContext {
    pub fn set_session_file(&self, file: Option<&Path>) {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).0 = file.map(|f| f.display().to_string());
    }

    pub fn set_cwd(&self, cwd: &Path) {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).1 = cwd.display().to_string();
    }

    fn snapshot(&self) -> (Option<String>, String) {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }
}

/// The panic payload text.
pub fn panic_message(info: &std::panic::PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    let text = payload
        .downcast_ref::<&str>()
        .map(|text| text.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panic".into());
    match info.location() {
        Some(location) => format!("{text} at {}:{}", location.file(), location.line()),
        None => text,
    }
}

/// Record panics as `fatal_error` crashes in `path`, then run the earlier hook.
pub fn install_crash_hook(path: PathBuf, context: CrashContext) {
    let earlier = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let (session_file, cwd) = context.snapshot();
        let stack = std::backtrace::Backtrace::force_capture().to_string();
        let crash =
            Crash { kind: "fatal_error".into(), message: panic_message(info), stack: Some(stack), session_file, cwd };
        record_crash(crash, &path);
        earlier(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crash(message: &str) -> Crash {
        Crash { kind: "uncaught_exception".into(), message: message.into(), cwd: "/w".into(), ..Crash::default() }
    }

    #[test]
    fn keeps_the_newest_five_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = crash_log_path(dir.path());
        for index in 0..7 {
            record_crash(crash(&format!("boom {index}")), &path).unwrap();
        }
        let records = read_crash_log(&path);
        assert_eq!(
            records.iter().map(|record| record.message.as_str()).collect::<Vec<_>>(),
            ["boom 2", "boom 3", "boom 4", "boom 5", "boom 6"]
        );
        assert_eq!(records[0].version, VERSION);
        let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw[0]["sessionFile"], Value::Null);
        assert!(raw[0].get("notified").is_none());
    }

    #[test]
    fn takes_the_newest_recent_crash_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = crash_log_path(dir.path());
        let old = r#"[{"timestamp": "2020-01-01T00:00:00.000Z", "version": "0.1.0", "kind": "fatal_error", "message": "old", "stack": null, "sessionFile": null, "cwd": "/"}, {"message": 3}]"#;
        std::fs::write(&path, old).unwrap();
        assert_eq!(read_crash_log(&path).len(), 1);
        let now = crate::session::parse_iso("2020-01-03T00:00:00.000Z").unwrap();
        record_crash(crash("new"), &path).unwrap();
        let later = pi_ai::now_ms();
        assert_eq!(take_unnotified_crash(&path, later).unwrap().message, "new");
        assert_eq!(take_unnotified_crash(&path, later), None);
        assert!(read_crash_log(&path).iter().all(|record| record.notified == Some(true)));
        std::fs::write(&path, old).unwrap();
        assert_eq!(take_unnotified_crash(&path, now).unwrap().message, "old");
        clear_crash_log(&path);
        assert!(read_crash_log(&path).is_empty());
        assert!(read_crash_log(&dir.path().join("missing.json")).is_empty());
    }
}
