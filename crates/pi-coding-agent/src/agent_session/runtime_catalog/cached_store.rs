//! `models-store.json` with the parsed document kept between reads. A refresh reads one entry
//! per provider; the cache parses the file once while its size and modification time stay put.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use async_trait::async_trait;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::catalog::{FileModelsStore, ModelsStore, ModelsStoreEntry};

type Snapshot = (Option<SystemTime>, u64, Map<String, Value>);

pub struct CachedFileModelsStore {
    inner: FileModelsStore,
    cache: Mutex<Option<Snapshot>>,
}

impl CachedFileModelsStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { inner: FileModelsStore::new(path), cache: Mutex::new(None) }
    }

    pub fn path(&self) -> &Path {
        self.inner.path()
    }

    fn cached_entry(&self, provider_id: &str) -> Option<Option<ModelsStoreEntry>> {
        let metadata = std::fs::metadata(self.path()).ok()?;
        let stamp = (metadata.modified().ok(), metadata.len());
        let mut cache = self.cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let fresh = cache.as_ref().is_some_and(|(modified, len, _)| (*modified, *len) == stamp);
        if !fresh {
            let text = std::fs::read_to_string(self.path()).ok()?;
            let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
            let document: Map<String, Value> =
                if text.trim().is_empty() { Map::new() } else { serde_json::from_str(text).ok()? };
            *cache = Some((stamp.0, stamp.1, document));
        }
        let (_, _, document) = cache.as_ref()?;
        Some(document.get(provider_id).and_then(|value| serde_json::from_value(value.clone()).ok()))
    }

    fn invalidate(&self) {
        *self.cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }
}

#[async_trait]
impl ModelsStore for CachedFileModelsStore {
    async fn read(&self, provider_id: &str, cancel: &CancellationToken) -> anyhow::Result<Option<ModelsStoreEntry>> {
        anyhow::ensure!(!cancel.is_cancelled(), "Models store operation cancelled");
        match self.cached_entry(provider_id) {
            Some(entry) => Ok(entry),
            // A missing, unreadable, or invalid file: the plain store reports it as before.
            None => self.inner.read(provider_id, cancel).await,
        }
    }

    async fn write(
        &self,
        provider_id: &str,
        entry: &ModelsStoreEntry,
        cancel: &CancellationToken,
    ) -> anyhow::Result<()> {
        self.invalidate();
        let result = self.inner.write(provider_id, entry, cancel).await;
        self.invalidate();
        result
    }

    async fn delete(&self, provider_id: &str, cancel: &CancellationToken) -> anyhow::Result<()> {
        self.invalidate();
        let result = self.inner.delete(provider_id, cancel).await;
        self.invalidate();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_follow_writes_and_external_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models-store.json");
        let store = CachedFileModelsStore::new(&path);
        let cancel = CancellationToken::new();
        assert_eq!(store.read("p", &cancel).await.unwrap(), None);
        let entry = ModelsStoreEntry { checked_at: Some(1), ..ModelsStoreEntry::default() };
        store.write("p", &entry, &cancel).await.unwrap();
        assert_eq!(store.read("p", &cancel).await.unwrap(), Some(entry.clone()));
        assert_eq!(store.read("q", &cancel).await.unwrap(), None);
        std::fs::write(&path, r#"{"q": {"models": [], "checkedAt": 22}}"#).unwrap();
        assert_eq!(store.read("q", &cancel).await.unwrap().and_then(|entry| entry.checked_at), Some(22));
        store.delete("q", &cancel).await.unwrap();
        assert_eq!(store.read("q", &cancel).await.unwrap(), None);
    }
}
