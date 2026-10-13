//! In-memory session store with revision compare-and-swap.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::host::SessionStore;
use crate::types::{SessionSnapshot, SessionSummary};

#[derive(Default)]
pub struct MemorySessionStore {
    snapshots: Mutex<BTreeMap<String, SessionSnapshot>>,
}

impl MemorySessionStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl SessionStore for MemorySessionStore {
    async fn load(&self, id: &str) -> anyhow::Result<Option<SessionSnapshot>> {
        Ok(self.snapshots.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get(id).cloned())
    }

    async fn list(&self) -> anyhow::Result<Vec<SessionSummary>> {
        let snapshots = self.snapshots.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(snapshots
            .values()
            .map(|snapshot| SessionSummary {
                id: snapshot.id.clone(),
                status: snapshot.status,
                revision: snapshot.revision,
            })
            .collect())
    }

    async fn save(&self, snapshot: &SessionSnapshot, expected_revision: Option<u64>) -> anyhow::Result<()> {
        let mut snapshots = self.snapshots.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let current = snapshots.get(&snapshot.id).map(|current| current.revision);
        if current != expected_revision {
            anyhow::bail!("Session revision conflict");
        }
        if let Some(expected) = expected_revision
            && snapshot.revision != expected + 1
        {
            anyhow::bail!("Invalid session revision");
        }
        snapshots.insert(snapshot.id.clone(), snapshot.clone());
        Ok(())
    }
}
