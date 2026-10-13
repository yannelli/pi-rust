//! Persisted provider catalogs: `models-store.json` in the agent directory, keyed by provider
//! id and locked like `auth.json` (upstream `core/models-store.ts`, `ai/src/models-store.ts`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Context as _;
use async_trait::async_trait;
use pi_ai::Model;
use pi_ai::auth::store_lock::FileLock;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

/// Model types this version understands; stored and remote models of other types are dropped.
pub const KNOWN_MODEL_TYPES: [&str; 3] = ["chat", "image", "classifier"];

/// `type`, defaulting to `chat`.
pub fn model_type(model: &Model) -> &str {
    model.model_type.as_deref().unwrap_or("chat")
}

pub fn has_known_model_type(model: &Model) -> bool {
    KNOWN_MODEL_TYPES.contains(&model_type(model))
}

fn lenient_models<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<Model>, D::Error> {
    let values = Vec::<Value>::deserialize(deserializer)?;
    Ok(values.into_iter().filter_map(|value| serde_json::from_value(value).ok()).collect())
}

/// One provider's persisted catalog and its remote validators.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelsStoreEntry {
    #[serde(default, deserialize_with = "lenient_models")]
    pub models: Vec<Model>,
    /// Unix milliseconds of the remote catalog date header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<i64>,
    /// Unix milliseconds of the last completed remote check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<i64>,
    /// The remote `ETag`, quotes included, sent back as `If-None-Match`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
}

impl ModelsStoreEntry {
    pub fn with_models(models: Vec<Model>) -> Self {
        Self { models, ..Self::default() }
    }

    /// The entry without models of unknown types.
    pub fn with_known_model_types(mut self) -> Self {
        self.models.retain(has_known_model_type);
        self
    }
}

#[async_trait]
pub trait ModelsStore: Send + Sync {
    async fn read(&self, provider_id: &str, cancel: &CancellationToken) -> anyhow::Result<Option<ModelsStoreEntry>>;
    async fn write(
        &self,
        provider_id: &str,
        entry: &ModelsStoreEntry,
        cancel: &CancellationToken,
    ) -> anyhow::Result<()>;
    async fn delete(&self, provider_id: &str, cancel: &CancellationToken) -> anyhow::Result<()>;
}

fn check_cancel(cancel: &CancellationToken) -> anyhow::Result<()> {
    anyhow::ensure!(!cancel.is_cancelled(), "Models store operation cancelled");
    Ok(())
}

#[derive(Default)]
pub struct InMemoryModelsStore {
    entries: Mutex<HashMap<String, ModelsStoreEntry>>,
}

impl InMemoryModelsStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ModelsStore for InMemoryModelsStore {
    async fn read(&self, provider_id: &str, cancel: &CancellationToken) -> anyhow::Result<Option<ModelsStoreEntry>> {
        check_cancel(cancel)?;
        Ok(self.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get(provider_id).cloned())
    }

    async fn write(
        &self,
        provider_id: &str,
        entry: &ModelsStoreEntry,
        cancel: &CancellationToken,
    ) -> anyhow::Result<()> {
        check_cancel(cancel)?;
        let mut entries = self.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        entries.insert(provider_id.into(), entry.clone());
        Ok(())
    }

    async fn delete(&self, provider_id: &str, cancel: &CancellationToken) -> anyhow::Result<()> {
        check_cancel(cancel)?;
        self.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(provider_id);
        Ok(())
    }
}

/// `models-store.json`. Entries of other providers stay as they are.
pub struct FileModelsStore {
    path: PathBuf,
}

/// `<agent dir>/models-store.json`.
pub fn models_store_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("models-store.json")
}

impl FileModelsStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read_document(&self) -> anyhow::Result<Map<String, Value>> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
            Err(error) => return Err(error).with_context(|| format!("cannot read {}", self.path.display())),
        };
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        if text.trim().is_empty() {
            return Ok(Map::new());
        }
        serde_json::from_str(text).with_context(|| format!("invalid models store {}", self.path.display()))
    }

    fn write_document(&self, document: &Map<String, Value>) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(document)?;
        let mut temp = self.path.clone().into_os_string();
        temp.push(format!(".{}.tmp", std::process::id()));
        let temp = PathBuf::from(temp);
        write_private(&temp, &text)?;
        std::fs::rename(&temp, &self.path).inspect_err(|_| {
            let _ = std::fs::remove_file(&temp);
        })?;
        Ok(())
    }

    async fn update(
        &self,
        cancel: &CancellationToken,
        change: impl FnOnce(&mut Map<String, Value>),
    ) -> anyhow::Result<()> {
        let lock = FileLock::acquire(&self.path, cancel).await.map_err(|error| anyhow::anyhow!("{error}"))?;
        check_cancel(cancel)?;
        let mut document = self.read_document()?;
        change(&mut document);
        let result = self.write_document(&document);
        drop(lock);
        result
    }
}

fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(text.as_bytes())
}

#[async_trait]
impl ModelsStore for FileModelsStore {
    async fn read(&self, provider_id: &str, cancel: &CancellationToken) -> anyhow::Result<Option<ModelsStoreEntry>> {
        check_cancel(cancel)?;
        let document = self.read_document()?;
        check_cancel(cancel)?;
        Ok(document.get(provider_id).and_then(|value| serde_json::from_value(value.clone()).ok()))
    }

    async fn write(
        &self,
        provider_id: &str,
        entry: &ModelsStoreEntry,
        cancel: &CancellationToken,
    ) -> anyhow::Result<()> {
        let value = serde_json::to_value(entry)?;
        self.update(cancel, |document| {
            document.insert(provider_id.into(), value);
        })
        .await
    }

    async fn delete(&self, provider_id: &str, cancel: &CancellationToken) -> anyhow::Result<()> {
        self.update(cancel, |document| {
            document.remove(provider_id);
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(provider: &str, id: &str) -> Model {
        serde_json::from_value(serde_json::json!({"id": id, "name": id, "api": "openai-completions",
            "provider": provider, "baseUrl": "https://example.test/v1"}))
        .unwrap()
    }

    #[tokio::test]
    async fn file_store_keeps_unrelated_providers() {
        let dir = tempfile::tempdir().unwrap();
        let path = models_store_path(dir.path());
        std::fs::write(&path, "\u{feff}{\"other\": {\"models\": [], \"custom\": {\"keep\": true}}}").unwrap();
        let store = FileModelsStore::new(&path);
        let cancel = CancellationToken::new();
        let entry = ModelsStoreEntry {
            models: vec![model("p", "a")],
            last_modified: Some(1),
            checked_at: Some(2),
            etag: Some("\"v1\"".into()),
        };
        store.write("p", &entry, &cancel).await.unwrap();
        assert_eq!(store.read("p", &cancel).await.unwrap(), Some(entry));
        let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["other"]["custom"]["keep"], true);
        assert_eq!(raw["p"]["lastModified"], 1);
        assert_eq!(raw["p"]["etag"], "\"v1\"");
        assert!(!pi_ai::auth::store_lock::lock_dir(&path).exists());

        store.delete("p", &cancel).await.unwrap();
        assert_eq!(store.read("p", &cancel).await.unwrap(), None);
        assert!(store.read("other", &cancel).await.unwrap().is_some());
        assert_eq!(FileModelsStore::new(dir.path().join("missing.json")).read("p", &cancel).await.unwrap(), None);
    }

    #[tokio::test]
    async fn invalid_models_are_skipped_and_cancellation_stops_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models-store.json");
        let good = serde_json::to_value(model("p", "good")).unwrap();
        let text = serde_json::json!({"p": {"models": [good, {"id": "broken"}], "checkedAt": 5}}).to_string();
        std::fs::write(&path, text).unwrap();
        let store = FileModelsStore::new(&path);
        let entry = store.read("p", &CancellationToken::new()).await.unwrap().unwrap();
        assert_eq!(entry.models.len(), 1);
        assert_eq!(entry.checked_at, Some(5));

        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(store.write("q", &ModelsStoreEntry::default(), &cancel).await.is_err());
        assert!(store.read("q", &CancellationToken::new()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn concurrent_writers_do_not_lose_providers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models-store.json");
        let writes = (0..6).map(|index| {
            let store = FileModelsStore::new(&path);
            async move {
                let entry = ModelsStoreEntry::with_models(vec![model("p", &format!("m{index}"))]);
                store.write(&format!("p{index}"), &entry, &CancellationToken::new()).await.unwrap();
            }
        });
        futures::future::join_all(writes).await;
        let document: Map<String, Value> = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(document.len(), 6);
    }

    #[test]
    fn unknown_model_types_are_dropped() {
        let mut video = model("p", "clip");
        video.model_type = Some("video".into());
        let mut image = model("p", "flux");
        image.model_type = Some("image".into());
        let entry = ModelsStoreEntry::with_models(vec![model("p", "chat"), image, video]).with_known_model_types();
        let ids: Vec<&str> = entry.models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, ["chat", "flux"]);
    }
}
