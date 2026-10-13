//! The ACP session map (upstream `session-store.ts`): `~/.pi/pi-acp/session-map.json` maps
//! ACP session ids to pi session files so `session/load` can reattach.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::agent_session::lock;
use crate::session::ids::iso_timestamp;

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
    crate::config::home_dir().join(".pi").join("pi-acp").join("session-map.json")
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
        if let Err(error) = std::fs::write(&self.path, text) {
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
