//! Port of upstream `durable/sessions.ts` (and `vacation/sessions.ts`): session directories and the lock.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pi_durable::{DurableError, DurableResult};
use regex::Regex;

use crate::experimental::lockfile::{LockOptions, LockRelease, lock};
use crate::session::ids::uuid_v4;

/// One session directory holding `session.sqlite`, locked by this process.
pub struct SessionLocation {
    pub id: String,
    pub directory: PathBuf,
    pub database: PathBuf,
    pub cwd: String,
    pub created: bool,
    release: Option<LockRelease>,
}

impl SessionLocation {
    pub async fn release(&mut self) -> DurableResult<()> {
        match self.release.take() {
            Some(release) => release.release().await.map_err(|error| DurableError::Error(error.to_string())),
            None => Ok(()),
        }
    }
}

fn hash_prefix(cwd: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, cwd.as_bytes());
    digest.as_ref().iter().map(|byte| format!("{byte:02x}")).collect::<String>()[..24].to_string()
}

fn io(error: std::io::Error) -> DurableError {
    DurableError::Error(error.to_string())
}

/// A new session for `cwd`, or its newest one with `continue_session`. `kind` names the app: sessions live
/// under `<agent dir>/experimental/<kind>-sessions/<cwd hash>/`.
pub async fn select_session(
    agent_dir: &Path,
    kind: &str,
    cwd: &Path,
    continue_session: bool,
) -> DurableResult<SessionLocation> {
    let cwd = tokio::fs::canonicalize(std::path::absolute(cwd).map_err(io)?).await.map_err(io)?;
    let cwd = cwd.display().to_string();
    let root = agent_dir.join("experimental").join(format!("{kind}-sessions")).join(hash_prefix(&cwd));
    tokio::fs::create_dir_all(&root).await.map_err(io)?;

    let (directory, created) = if continue_session {
        let pattern = Regex::new(r"^\d{13}-[0-9a-f-]{36}$").unwrap_or_else(|_| unreachable!("valid pattern"));
        let mut names = Vec::new();
        let mut entries = tokio::fs::read_dir(&root).await.map_err(io)?;
        while let Some(entry) = entries.next_entry().await.map_err(io)? {
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type().await.map_err(io)?.is_dir() && pattern.is_match(&name) {
                names.push(name);
            }
        }
        names.sort();
        let Some(newest) = names.pop() else {
            return Err(DurableError::Error(format!("No {kind} session exists for {cwd}")));
        };
        (root.join(newest), false)
    } else {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
        let directory = root.join(format!("{now:013}-{}", uuid_v4()));
        tokio::fs::create_dir(&directory).await.map_err(io)?;
        (directory, true)
    };

    // A lock left by a crashed process goes stale after 10 s; wait that long before giving up.
    let options = LockOptions {
        realpath: false,
        stale: Duration::from_secs(10),
        update: Duration::from_secs(5),
        retry_interval: Duration::from_secs(1),
        max_retry_time: Duration::from_secs(12),
    };
    let release = lock(&directory, options).await.map_err(|error| {
        DurableError::Error(format!("Session is already open in another process: {} ({error})", directory.display()))
    })?;
    let id = directory.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let database = directory.join("session.sqlite");
    Ok(SessionLocation { id, directory, database, cwd, created, release: Some(release) })
}
