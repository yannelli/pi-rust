//! The server's Session catalog (upstream `experimental/session-catalog.ts`): one directory per Session
//! holding `meta.json` and the worker-owned `session.sqlite`.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use pi_chord::{ChordError, ChordResult};
use regex::Regex;
use serde::{Deserialize, Serialize};

/// One server-hosted Session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCatalogMetadata {
    pub id: String,
    pub created_at: f64,
    /// The working directory the Session's agent runs in.
    pub cwd: String,
    /// The Session directory. Workers lock it and own the storage inside it.
    pub path: String,
}

impl pi_server::SessionMetadata for SessionCatalogMetadata {
    fn id(&self) -> &str {
        &self.id
    }
}

const METADATA_FILE: &str = "meta.json";
const STORAGE_FILE: &str = "session.sqlite";
static SESSION_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$").expect("valid pattern"));

pub fn is_session_id(id: &str) -> bool {
    SESSION_ID.is_match(id)
}

/// The durable storage file of a Session. Only the Session's worker opens it.
pub fn session_storage_path(metadata: &SessionCatalogMetadata) -> PathBuf {
    Path::new(&metadata.path).join(STORAGE_FILE)
}

/// Every Session in the directory. Entries without valid metadata are skipped.
pub async fn list_sessions(session_dir: &Path) -> ChordResult<Vec<SessionCatalogMetadata>> {
    let mut entries = match tokio::fs::read_dir(session_dir).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(ChordError::other(error)),
    };
    let mut names = Vec::new();
    while let Some(entry) = entries.next_entry().await.map_err(ChordError::other)? {
        if let Some(name) = entry.file_name().to_str().filter(|name| is_session_id(name)) {
            names.push(name.to_string());
        }
    }
    names.sort();
    let mut sessions = Vec::new();
    for name in names {
        if let Some(metadata) = read_session(session_dir, &name).await {
            sessions.push(metadata);
        }
    }
    Ok(sessions)
}

/// One Session by ID, or `None` when it does not exist.
pub async fn read_session(session_dir: &Path, id: &str) -> Option<SessionCatalogMetadata> {
    if !is_session_id(id) {
        return None;
    }
    let path = session_dir.join(id);
    let text = tokio::fs::read_to_string(path.join(METADATA_FILE)).await.ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let created_at = value.get("createdAt")?.as_f64()?;
    let cwd = value.get("cwd")?.as_str()?.to_string();
    Some(SessionCatalogMetadata { id: id.to_string(), created_at, cwd, path: path.display().to_string() })
}

pub fn now_ms() -> f64 {
    let elapsed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    elapsed.as_millis() as f64
}

/// Create an empty Session. Its worker creates the storage on first open.
pub async fn create_session(session_dir: &Path, id: Option<&str>, cwd: &str) -> ChordResult<SessionCatalogMetadata> {
    let id = id.map_or_else(pi_server::random_uuid, str::to_string);
    if !is_session_id(&id) {
        return Err(ChordError::error(format!("Invalid session ID: {id}")));
    }
    let path = session_dir.join(&id);
    tokio::fs::create_dir_all(session_dir).await.map_err(ChordError::other)?;
    match tokio::fs::create_dir(&path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(ChordError::error(format!("Session {id} already exists")));
        }
        Err(error) => return Err(ChordError::other(error)),
    }
    let metadata =
        SessionCatalogMetadata { id, created_at: now_ms(), cwd: cwd.to_string(), path: path.display().to_string() };
    let created_at = pi_chord::Json::from_f64(metadata.created_at).unwrap_or(pi_chord::Json::Null);
    let cwd = serde_json::Value::from(metadata.cwd.as_str());
    let text = format!("{{\n\t\"createdAt\": {created_at},\n\t\"cwd\": {cwd}\n}}\n");
    tokio::fs::write(path.join(METADATA_FILE), text).await.map_err(ChordError::other)?;
    Ok(metadata)
}

/// Delete a Session directory. Its worker must be closed first.
pub async fn delete_session(metadata: &SessionCatalogMetadata) -> ChordResult<()> {
    match tokio::fs::remove_dir_all(&metadata.path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ChordError::other(error)),
    }
}
