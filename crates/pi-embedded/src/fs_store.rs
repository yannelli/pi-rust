//! File-backed session checkpoints (upstream Node `NodeSessionStore`).

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::fs_util::{atomic_write, ensure_private_dir, read_optional, serialized, sha256_hex};
use crate::host::SessionStore;
use crate::types::{SessionSnapshot, SessionSummary};

/// One `<sha256(id)>.json` checkpoint per session, written atomically, with process-local
/// compare-and-swap across store instances. Cross-process hosts supply their own coordination.
pub struct FileSessionStore {
    directory: PathBuf,
}

impl FileSessionStore {
    pub fn new(directory: impl AsRef<Path>) -> Self {
        let directory = directory.as_ref();
        let directory = std::path::absolute(directory).unwrap_or_else(|_| directory.to_path_buf());
        Self { directory }
    }

    fn path(&self, id: &str) -> PathBuf {
        self.directory.join(format!("{}.json", sha256_hex(id)))
    }
}

fn parse(content: &str, name: &str) -> anyhow::Result<SessionSnapshot> {
    serde_json::from_str(content).map_err(|error| anyhow::anyhow!("Invalid session checkpoint: {name}: {error}"))
}

fn is_checkpoint_name(name: &str) -> bool {
    name.len() == 69
        && name.ends_with(".json")
        && name[..64].bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

#[async_trait]
impl SessionStore for FileSessionStore {
    async fn load(&self, id: &str) -> anyhow::Result<Option<SessionSnapshot>> {
        let Some(content) = read_optional(&self.path(id)).await? else { return Ok(None) };
        let snapshot = parse(&content, id)?;
        if snapshot.version != 1 || snapshot.id != id {
            anyhow::bail!("Invalid session checkpoint: {id}");
        }
        Ok(Some(snapshot))
    }

    async fn list(&self) -> anyhow::Result<Vec<SessionSummary>> {
        ensure_private_dir(&self.directory).await?;
        let mut summaries = Vec::new();
        let mut entries = tokio::fs::read_dir(&self.directory).await?;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !is_checkpoint_name(&name) {
                continue;
            }
            let snapshot = parse(&tokio::fs::read_to_string(entry.path()).await?, &name)?;
            if snapshot.version != 1 || name != format!("{}.json", sha256_hex(&snapshot.id)) {
                anyhow::bail!("Invalid session checkpoint: {name}");
            }
            summaries.push(SessionSummary { id: snapshot.id, status: snapshot.status, revision: snapshot.revision });
        }
        summaries.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(summaries)
    }

    async fn save(&self, snapshot: &SessionSnapshot, expected_revision: Option<u64>) -> anyhow::Result<()> {
        ensure_private_dir(&self.directory).await?;
        let directory = tokio::fs::canonicalize(&self.directory).await?;
        let id = snapshot.id.clone();
        let key = directory.join(sha256_hex(&id)).to_string_lossy().into_owned();
        serialized(&key, async {
            let current = self.load(&id).await?;
            if current.map(|current| current.revision) != expected_revision {
                anyhow::bail!("Session revision conflict: {id}");
            }
            if snapshot.version != 1 {
                anyhow::bail!("Invalid session checkpoint: {id}");
            }
            if let Some(expected) = expected_revision
                && snapshot.revision != expected + 1
            {
                anyhow::bail!("Session revision must advance by one: {id}");
            }
            let path = directory.join(format!("{}.json", sha256_hex(&id)));
            atomic_write(&path, &serde_json::to_string(snapshot)?, 0o600).await
        })
        .await
    }
}
