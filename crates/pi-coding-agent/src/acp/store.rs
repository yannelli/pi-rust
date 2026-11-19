//! The ACP session map (upstream `session-store.ts`): `~/.pi/pi-acp/session-map.json` maps
//! ACP session ids to pi session files so `session/load` can reattach.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::agent_session::lock;
use crate::resources::brand::CONFIG_DIR_NAME;
use crate::session::ids::iso_timestamp;
use crate::session::{CURRENT_SESSION_VERSION, SessionHeader};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// Replace `path` with `text` through a temporary file in the same directory, so a reader in
/// another pi process never sees a partly written file.
fn write_atomically(path: &Path, text: &str) -> std::io::Result<()> {
    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let temp = path.with_file_name(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let result = std::fs::write(&temp, text).and_then(|()| std::fs::rename(&temp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// Start the session file of a session that has none yet, such as one that never got a prompt:
/// the header line pi writes, with this id and cwd. Without it pi would open a session with
/// another id and its own working directory. An existing file is left alone.
pub fn start_missing_session_file(path: &Path, session_id: &str, cwd: &str) {
    let empty = std::fs::metadata(path).map(|metadata| metadata.len() == 0);
    if path.as_os_str().is_empty() || matches!(empty, Ok(false)) {
        return;
    }
    let header = SessionHeader {
        kind: "session".into(),
        version: Some(CURRENT_SESSION_VERSION),
        id: session_id.into(),
        timestamp: iso_timestamp(),
        cwd: cwd.into(),
        parent_session: None,
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let line = serde_json::to_string(&header).unwrap_or_default() + "\n";
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    if empty.is_ok() {
        options.truncate(true);
    } else {
        // Another pi process may create it meanwhile.
        options.create_new(true);
    }
    if let Err(error) = options.open(path).and_then(|mut file| file.write_all(line.as_bytes())) {
        eprintln!("Warning: could not start the session file {}: {error}", path.display());
    }
}

/// One mapped session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredSession {
    pub session_id: String,
    pub cwd: String,
    pub session_file: String,
    pub updated_at: String,
}

/// `~/.pi/pi-acp/session-map.json`, kept apart from pi's own `~/.pi/agent`.
pub fn default_session_map_path() -> PathBuf {
    crate::config::home_dir().join(CONFIG_DIR_NAME).join("pi-acp").join("session-map.json")
}

/// The map file, read and rewritten on every change.
pub struct SessionStore {
    path: PathBuf,
    write: Mutex<()>,
}

impl SessionStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), write: Mutex::new(()) }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load(&self) -> Map<String, Value> {
        let parsed =
            std::fs::read_to_string(&self.path).ok().and_then(|text| serde_json::from_str::<Value>(&text).ok());
        match parsed {
            Some(Value::Object(mut file)) if file.get("version") == Some(&json!(1)) => match file.remove("sessions") {
                Some(Value::Object(sessions)) => sessions,
                _ => Map::new(),
            },
            _ => Map::new(),
        }
    }

    fn save(&self, sessions: Map<String, Value>) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = json!({"version": 1, "sessions": sessions});
        let text = serde_json::to_string_pretty(&file).unwrap_or_default() + "\n";
        if let Err(error) = write_atomically(&self.path, &text) {
            eprintln!("Warning: could not write {}: {error}", self.path.display());
        }
    }

    pub fn get(&self, session_id: &str) -> Option<StoredSession> {
        self.load().get(session_id).and_then(|entry| serde_json::from_value(entry.clone()).ok())
    }

    /// Record a session with the current time as `updatedAt`.
    pub fn upsert(&self, session_id: &str, cwd: &str, session_file: &str) {
        let _guard = lock(&self.write);
        let mut sessions = self.load();
        let entry = StoredSession {
            session_id: session_id.into(),
            cwd: cwd.into(),
            session_file: session_file.into(),
            updated_at: iso_timestamp(),
        };
        sessions.insert(session_id.into(), serde_json::to_value(entry).unwrap_or(Value::Null));
        self.save(sessions);
    }

    pub fn delete(&self, session_id: &str) {
        let _guard = lock(&self.write);
        let mut sessions = self.load();
        if sessions.remove(session_id).is_some() {
            self.save(sessions);
        }
    }
}
