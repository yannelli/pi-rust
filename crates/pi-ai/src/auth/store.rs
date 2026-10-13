//! Credential stores: `auth.json` on disk with a cross-process lock (upstream
//! `coding-agent/src/core/auth-storage.ts`), an in-memory store, and an overlay for
//! per-run API keys such as `--api-key`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;

use async_trait::async_trait;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use super::resolve_value::{is_command_config_value, resolve_config_value};
use super::store_lock::FileLock;
use super::{ApiKeyCredential, AuthError, AuthResult, AuthType, Credential, CredentialInfo, CredentialStore, UpdateFn};

type Revision = Option<(SystemTime, u64)>;

#[derive(Default)]
struct ReadState {
    data: Map<String, Value>,
    revision: Revision,
    loaded: bool,
}

/// `auth.json` store. Writes hold the `auth.json.lock` directory lock, which the TypeScript
/// pi also uses, so both can share the file. Unknown entries and fields are preserved.
pub struct FileCredentialStore {
    path: PathBuf,
    read_only: bool,
    state: Mutex<ReadState>,
    writes: tokio::sync::Mutex<()>,
}

fn revision(path: &Path) -> Revision {
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.modified().ok()?, metadata.len()))
}

fn storage_error(message: impl Into<String>) -> AuthError {
    AuthError::Storage(message.into())
}

/// Parse `auth.json` text. A byte order mark is ignored; empty text is an empty store.
pub fn parse_auth_json(text: &str) -> AuthResult<Map<String, Value>> {
    let text = text.trim_start_matches('\u{feff}');
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(storage_error("Invalid auth.json: expected an object")),
        Err(error) => Err(storage_error(format!("Failed to read auth.json: {error}"))),
    }
}

fn parse_credential(provider_id: &str, value: &Value) -> AuthResult<Credential> {
    serde_json::from_value(value.clone())
        .map_err(|_| storage_error(format!("Invalid auth.json credential for provider \"{provider_id}\"")))
}

fn credential_info(provider_id: &str, value: &Value) -> Option<CredentialInfo> {
    let auth_type = match value.get("type").and_then(Value::as_str)? {
        "api_key" => AuthType::ApiKey,
        "oauth" => AuthType::OAuth,
        _ => return None,
    };
    Some(CredentialInfo { provider_id: provider_id.to_string(), auth_type })
}

/// Resolve a stored key (`$VAR`, `!command`) with the credential's own `env` first.
fn resolve_stored_key(credential: Credential, run_commands: bool) -> Credential {
    match credential {
        Credential::ApiKey(ApiKeyCredential { key: Some(key), env })
            if run_commands || !is_command_config_value(&key) =>
        {
            let key = resolve_config_value(&key, env.as_ref());
            Credential::ApiKey(ApiKeyCredential { key, env })
        }
        other => other,
    }
}

fn write_file(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(text.as_bytes())
}

fn create_parent(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) else { return Ok(()) };
    if parent.exists() {
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(parent)
}

impl FileCredentialStore {
    /// Store backed by `path`, normally `<agent dir>/auth.json`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), read_only: false, state: Mutex::default(), writes: tokio::sync::Mutex::new(()) }
    }

    /// Read-only view: never creates or writes the file, does not run `!command` keys, and
    /// rejects malformed entries anywhere in the file.
    pub fn read_only(path: impl Into<PathBuf>) -> Self {
        Self { read_only: true, ..Self::new(path) }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load(&self) -> AuthResult<Map<String, Value>> {
        let current = revision(&self.path);
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.loaded && current == state.revision {
            return Ok(state.data.clone());
        }
        let data = match std::fs::read_to_string(&self.path) {
            Ok(text) => parse_auth_json(&text)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Map::new(),
            Err(error) => return Err(storage_error(format!("Failed to read auth.json: {error}"))),
        };
        if self.read_only {
            for (provider_id, value) in &data {
                parse_credential(provider_id, value)?;
            }
        }
        *state = ReadState { data: data.clone(), revision: current, loaded: true };
        Ok(data)
    }

    fn remember(&self, data: Map<String, Value>) {
        let revision = revision(&self.path);
        *self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
            ReadState { data, revision, loaded: true };
    }

    /// Take the in-process and cross-process locks and read the current file contents.
    async fn begin_write(&self) -> AuthResult<(tokio::sync::MutexGuard<'_, ()>, FileLock, Map<String, Value>)> {
        if self.read_only {
            return Err(storage_error("Read-only credential storage cannot modify auth.json"));
        }
        let writes = self.writes.lock().await;
        create_parent(&self.path).map_err(|error| storage_error(format!("cannot create auth directory: {error}")))?;
        let lock = FileLock::acquire(&self.path, &CancellationToken::new()).await?;
        let data = match std::fs::read_to_string(&self.path) {
            Ok(text) => parse_auth_json(&text)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Map::new(),
            Err(error) => return Err(storage_error(format!("Failed to read auth.json: {error}"))),
        };
        Ok((writes, lock, data))
    }

    /// Write `data` while the locks from [`Self::begin_write`] are held.
    fn commit(&self, data: Map<String, Value>) -> AuthResult<()> {
        let text = serde_json::to_string_pretty(&data).map_err(|error| storage_error(error.to_string()))?;
        write_file(&self.path, &text).map_err(|error| storage_error(format!("cannot write auth.json: {error}")))?;
        self.remember(data);
        Ok(())
    }
}

#[async_trait]
impl CredentialStore for FileCredentialStore {
    async fn read(&self, provider_id: &str) -> AuthResult<Option<Credential>> {
        let data = self.load()?;
        let Some(value) = data.get(provider_id) else { return Ok(None) };
        Ok(Some(resolve_stored_key(parse_credential(provider_id, value)?, !self.read_only)))
    }

    async fn list(&self) -> AuthResult<Vec<CredentialInfo>> {
        Ok(self.load()?.iter().filter_map(|(provider_id, value)| credential_info(provider_id, value)).collect())
    }

    async fn update(&self, provider_id: &str, transform: UpdateFn) -> AuthResult<Option<Credential>> {
        let (_writes, _lock, mut data) = self.begin_write().await?;
        let current = data.get(provider_id).and_then(|value| serde_json::from_value::<Credential>(value.clone()).ok());
        match transform(current.clone()).await? {
            None => {
                self.remember(data);
                Ok(current)
            }
            Some(next) => {
                let value = serde_json::to_value(&next).map_err(|error| storage_error(error.to_string()))?;
                data.insert(provider_id.to_string(), value);
                self.commit(data)?;
                Ok(Some(next))
            }
        }
    }

    async fn delete(&self, provider_id: &str) -> AuthResult<()> {
        let (_writes, _lock, mut data) = self.begin_write().await?;
        data.remove(provider_id);
        self.commit(data)
    }
}

/// In-memory store with the same semantics, for tests and embedders.
#[derive(Default)]
pub struct InMemoryCredentialStore {
    credentials: tokio::sync::Mutex<BTreeMap<String, Credential>>,
}

impl InMemoryCredentialStore {
    pub fn new(credentials: BTreeMap<String, Credential>) -> Self {
        Self { credentials: tokio::sync::Mutex::new(credentials) }
    }
}

#[async_trait]
impl CredentialStore for InMemoryCredentialStore {
    async fn read(&self, provider_id: &str) -> AuthResult<Option<Credential>> {
        let credential = self.credentials.lock().await.get(provider_id).cloned();
        Ok(credential.map(|credential| resolve_stored_key(credential, true)))
    }

    async fn list(&self) -> AuthResult<Vec<CredentialInfo>> {
        let credentials = self.credentials.lock().await;
        Ok(credentials
            .iter()
            .map(|(provider_id, credential)| CredentialInfo {
                provider_id: provider_id.clone(),
                auth_type: credential.auth_type(),
            })
            .collect())
    }

    async fn update(&self, provider_id: &str, transform: UpdateFn) -> AuthResult<Option<Credential>> {
        let mut credentials = self.credentials.lock().await;
        let current = credentials.get(provider_id).cloned();
        match transform(current.clone()).await? {
            None => Ok(current),
            Some(next) => {
                credentials.insert(provider_id.to_string(), next.clone());
                Ok(Some(next))
            }
        }
    }

    async fn delete(&self, provider_id: &str) -> AuthResult<()> {
        self.credentials.lock().await.remove(provider_id);
        Ok(())
    }
}

/// Overlay of per-run API keys (upstream `RuntimeCredentials`). A runtime key wins over the
/// stored credential for reads and is never written to the inner store.
pub struct RuntimeCredentialStore {
    inner: Arc<dyn CredentialStore>,
    overrides: RwLock<HashMap<String, String>>,
}

impl RuntimeCredentialStore {
    pub fn new(inner: Arc<dyn CredentialStore>) -> Self {
        Self { inner, overrides: RwLock::default() }
    }

    pub fn set_runtime_api_key(&self, provider_id: &str, api_key: impl Into<String>) {
        self.overrides
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(provider_id.into(), api_key.into());
    }

    pub fn remove_runtime_api_key(&self, provider_id: &str) {
        self.overrides.write().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(provider_id);
    }

    pub fn has_runtime_api_key(&self, provider_id: &str) -> bool {
        self.overrides.read().unwrap_or_else(|poisoned| poisoned.into_inner()).contains_key(provider_id)
    }

    fn runtime_key(&self, provider_id: &str) -> Option<String> {
        self.overrides.read().unwrap_or_else(|poisoned| poisoned.into_inner()).get(provider_id).cloned()
    }

    pub fn inner(&self) -> &Arc<dyn CredentialStore> {
        &self.inner
    }
}

#[async_trait]
impl CredentialStore for RuntimeCredentialStore {
    async fn read(&self, provider_id: &str) -> AuthResult<Option<Credential>> {
        match self.runtime_key(provider_id) {
            Some(key) => Ok(Some(Credential::ApiKey(ApiKeyCredential { key: Some(key), env: None }))),
            None => self.inner.read(provider_id).await,
        }
    }

    async fn list(&self) -> AuthResult<Vec<CredentialInfo>> {
        let mut entries = self.inner.list().await?;
        let overrides: Vec<String> =
            self.overrides.read().unwrap_or_else(|poisoned| poisoned.into_inner()).keys().cloned().collect();
        for provider_id in overrides {
            entries.retain(|entry| entry.provider_id != provider_id);
            entries.push(CredentialInfo { provider_id, auth_type: AuthType::ApiKey });
        }
        Ok(entries)
    }

    async fn update(&self, provider_id: &str, transform: UpdateFn) -> AuthResult<Option<Credential>> {
        self.inner.update(provider_id, transform).await
    }

    async fn delete(&self, provider_id: &str) -> AuthResult<()> {
        self.inner.delete(provider_id).await?;
        self.remove_runtime_api_key(provider_id);
        Ok(())
    }
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
