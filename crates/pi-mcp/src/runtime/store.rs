//! Per-server OAuth state (client registration, tokens, pending PKCE verifier) in `mcp-auth.json`,
//! keyed by server name and URL so servers sharing a URL keep separate accounts.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Map, Value};
use url::Url;

use super::lock::{LockOptions, lock_async, lock_sync};
use crate::config::mcp_namespace;
use crate::error::OAuthError;
use crate::oauth::pkce::sha256;
use crate::oauth::{McpOAuthState, McpOAuthStateStore, OAuthTokens};

/// A refresh lock its holder stopped renewing (the process was killed) is taken over after this.
const REFRESH_LOCK_STALE: Duration = Duration::from_secs(20);
/// How long to wait for another process's refresh: longer than a stale lock lives.
const REFRESH_LOCK_WAIT: Duration = Duration::from_secs(25);
const REFRESH_LOCK_RETRY: Duration = Duration::from_millis(100);

/// Reads and replaces the stored text under a lock. `update` returns the text to write, if any.
pub trait AuthStorageBackend: Send + Sync {
    fn with_lock(&self, update: &mut dyn FnMut(Option<&str>) -> Option<String>) -> Result<(), String>;
}

/// A JSON file guarded by a `<file>.lock` directory, written with mode 0600.
pub struct FileAuthStorageBackend {
    path: PathBuf,
}

impl FileAuthStorageBackend {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    std::io::Write::write_all(&mut options.open(path)?, text.as_bytes())
}

impl AuthStorageBackend for FileAuthStorageBackend {
    fn with_lock(&self, update: &mut dyn FnMut(Option<&str>) -> Option<String>) -> Result<(), String> {
        if let Some(parent) = self.path.parent().filter(|parent| !parent.exists()) {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            builder.create(parent).map_err(|error| error.to_string())?;
        }
        if !self.path.exists() {
            write_private(&self.path, "{}").map_err(|error| error.to_string())?;
        }
        let options =
            LockOptions { stale: Duration::from_secs(10), retries: 9, retry_delay: Duration::from_millis(20) };
        let _lock = lock_sync(&self.path, &options)?;
        let current = std::fs::read_to_string(&self.path).ok();
        if let Some(next) = update(current.as_deref()) {
            write_private(&self.path, &next).map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct InMemoryAuthStorageBackend {
    value: Mutex<Option<String>>,
}

impl InMemoryAuthStorageBackend {
    pub fn with_content(content: impl Into<String>) -> Self {
        Self { value: Mutex::new(Some(content.into())) }
    }

    pub fn content(&self) -> Option<String> {
        self.value.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }
}

impl AuthStorageBackend for InMemoryAuthStorageBackend {
    fn with_lock(&self, update: &mut dyn FnMut(Option<&str>) -> Option<String>) -> Result<(), String> {
        let mut value = self.value.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(next) = update(value.as_deref()) {
            *value = Some(next);
        }
        Ok(())
    }
}

type States = Map<String, Value>;

fn parse_states(content: Option<&str>) -> Result<States, String> {
    let Some(content) = content.filter(|content| !content.trim().is_empty()) else { return Ok(States::new()) };
    let parsed: Value = serde_json::from_str(content).map_err(|error| format!("Invalid mcp-auth.json: {error}"))?;
    Ok(match parsed {
        Value::Object(states) => states,
        _ => States::new(),
    })
}

fn serialize_states(states: &States) -> String {
    format!("{}\n", serde_json::to_string_pretty(states).unwrap_or_else(|_| "{}".into()))
}

/// The key by name and URL, and the legacy key by URL alone that older versions wrote.
fn store_keys(name: &str, server_url: &str) -> (String, String) {
    let legacy = Url::parse(server_url).map(|url| url.to_string()).unwrap_or_else(|_| server_url.to_string());
    (format!("{}|{legacy}", mcp_namespace(name)), legacy)
}

#[derive(Clone)]
pub struct McpOAuthCredentialStore {
    backend: Arc<dyn AuthStorageBackend>,
    /// Directory of refresh lock files. Without one, refreshes are serialized in this process only.
    lock_dir: Option<PathBuf>,
}

impl McpOAuthCredentialStore {
    /// `mcp-auth.json` in the agent directory, with refresh locks next to it.
    pub fn new(agent_dir: &Path) -> Self {
        let backend = Arc::new(FileAuthStorageBackend::new(agent_dir.join("mcp-auth.json")));
        Self { backend, lock_dir: Some(agent_dir.to_path_buf()) }
    }

    pub fn with_backend(backend: Arc<dyn AuthStorageBackend>, lock_dir: Option<PathBuf>) -> Self {
        Self { backend, lock_dir }
    }

    fn update<T>(&self, mut change: impl FnMut(&mut States) -> (T, bool)) -> Result<T, String> {
        let mut result = None;
        let mut failure = None;
        self.backend.with_lock(&mut |current| match parse_states(current) {
            Ok(mut states) => {
                let (value, write) = change(&mut states);
                result = Some(value);
                write.then(|| serialize_states(&states))
            }
            Err(error) => {
                failure = Some(error);
                None
            }
        })?;
        match (result, failure) {
            (Some(result), _) => Ok(result),
            (None, failure) => Err(failure.unwrap_or_else(|| "mcp-auth.json could not be read".into())),
        }
    }

    pub fn for_server(&self, name: &str, server_url: &str) -> Arc<McpOAuthServerStore> {
        let (key, legacy_key) = store_keys(name, server_url);
        Arc::new(McpOAuthServerStore { store: self.clone(), key, legacy_key, local: tokio::sync::Mutex::new(()) })
    }

    /// Stored tokens, for noticing sign-ins done by another process. Does not take over legacy state.
    pub fn tokens(&self, name: &str, server_url: &str) -> Option<OAuthTokens> {
        let (key, legacy_key) = store_keys(name, server_url);
        let state =
            self.update(|states| (states.get(&key).or_else(|| states.get(&legacy_key)).cloned(), false)).ok()??;
        serde_json::from_value::<McpOAuthState>(state).ok()?.tokens
    }

    /// Whether credentials were stored. Also removes legacy state the server would take over.
    pub fn remove(&self, name: &str, server_url: &str) -> bool {
        let (key, legacy_key) = store_keys(name, server_url);
        self.update(|states| {
            let removed = states.shift_remove(&key).or_else(|| states.shift_remove(&legacy_key)).is_some();
            (removed, removed)
        })
        .unwrap_or(false)
    }
}

/// The state of one server, and the lock that keeps refreshes of its tokens apart.
pub struct McpOAuthServerStore {
    store: McpOAuthCredentialStore,
    key: String,
    legacy_key: String,
    local: tokio::sync::Mutex<()>,
}

impl McpOAuthServerStore {
    /// Run `work` while no other refresh of this server's tokens runs, in this or another process.
    pub async fn with_refresh_lock<T>(&self, work: impl Future<Output = T>) -> Result<T, OAuthError> {
        let _local = self.local.lock().await;
        let Some(dir) = &self.store.lock_dir else { return Ok(work.await) };
        let _ = std::fs::create_dir_all(dir);
        let hash: String = sha256(self.key.as_bytes())[..8].iter().map(|byte| format!("{byte:02x}")).collect();
        let options = LockOptions {
            stale: REFRESH_LOCK_STALE,
            retries: (REFRESH_LOCK_WAIT.as_millis() / REFRESH_LOCK_RETRY.as_millis()) as u32,
            retry_delay: REFRESH_LOCK_RETRY,
        };
        let path = dir.join(format!("mcp-auth-refresh-{hash}"));
        let _lock = lock_async(&path, &options).await.map_err(OAuthError::Other)?;
        Ok(work.await)
    }
}

#[async_trait]
impl McpOAuthStateStore for McpOAuthServerStore {
    /// The first server to load legacy state takes it over; others with the same URL sign in again.
    async fn load(&self) -> Result<Option<McpOAuthState>, OAuthError> {
        let state = self
            .store
            .update(|states| {
                if states.contains_key(&self.key) || !states.contains_key(&self.legacy_key) {
                    return (states.get(&self.key).cloned(), false);
                }
                let legacy = states.shift_remove(&self.legacy_key);
                if let Some(legacy) = &legacy {
                    states.insert(self.key.clone(), legacy.clone());
                }
                (legacy, true)
            })
            .map_err(OAuthError::Other)?;
        Ok(state.and_then(|state| serde_json::from_value(state).ok()))
    }

    async fn save(&self, state: McpOAuthState) -> Result<(), OAuthError> {
        let value = serde_json::to_value(&state).map_err(|error| OAuthError::other(error.to_string()))?;
        self.store
            .update(|states| {
                states.insert(self.key.clone(), value.clone());
                ((), true)
            })
            .map_err(OAuthError::Other)
    }
}
