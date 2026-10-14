//! Experimental server profiles, activation, and lifetime (upstream `experimental/server.ts`). Starting
//! a server is in `server_runtime`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_chord::{ChordError, ChordResult};
use tokio::task::JoinHandle;

use super::lockfile::{LockOptions, LockRelease, lock as lock_file};
use super::plugins::package::resolve_path;
use super::process::lock;

pub const ENV_SERVER_DIR: &str = "PI_SERVER_DIR";
pub const ENV_SERVER_ID: &str = "PI_SERVER_ID";
const DEFAULT_SERVER_ID_FILE: &str = "default-server-id";
const LOCK_STALE: Duration = Duration::from_secs(30);
const LOCK_RETRY: Duration = Duration::from_millis(25);
const LOCK_WAIT: Duration = Duration::from_secs(30);
pub(super) const ACTIVATION_TIMEOUT: Duration = Duration::from_secs(10);
const AUTO_SERVER_STARTUP_GRACE: Duration = Duration::from_secs(10);
const AUTO_SERVER_IDLE_GRACE: Duration = Duration::from_secs(1);

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// The server profile and socket directory: `directory`, else `PI_SERVER_DIR`, else `~/.pi/server`.
pub fn resolve_server_directory(directory: Option<&str>) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let chosen = directory.map(str::to_string).or_else(|| env(ENV_SERVER_DIR));
    let path = match chosen {
        Some(path) => crate::config::expand_tilde(&path),
        None => home.join(".pi").join("server"),
    };
    resolve_path(&path)
}

/// The durable Session directory: `session_dir`, else `experimental/sessions` under the agent directory.
pub fn resolve_session_directory(session_dir: Option<&str>) -> PathBuf {
    match session_dir {
        Some(path) => resolve_path(&crate::config::expand_tilde(path)),
        None => resolve_path(&crate::config::agent_dir().join("experimental").join("sessions")),
    }
}

/// Create the directory as private (0700) and check that the current user owns it.
#[cfg(unix)]
pub async fn ensure_private_server_directory(directory: &Path) -> ChordResult<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(directory).map_err(ChordError::other)?;
    let metadata = std::fs::symlink_metadata(directory).map_err(ChordError::other)?;
    if !metadata.is_dir() {
        return Err(ChordError::error(format!("Unix socket directory is not a directory: {}", directory.display())));
    }
    if metadata.uid() != rustix::process::getuid().as_raw() {
        let message = format!("Unix socket directory is not owned by the current user: {}", directory.display());
        return Err(ChordError::error(message));
    }
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).map_err(ChordError::other)
}

/// One locked logical server ID in a shared server directory.
pub struct ServerProfile {
    pub server_id: String,
    lock: LockRelease,
}

impl ServerProfile {
    pub async fn release(self) -> ChordResult<()> {
        self.lock.release().await
    }
}

fn lock_options(stale: Duration, update: Duration, wait: Duration) -> LockOptions {
    LockOptions { realpath: false, stale, update, retry_interval: LOCK_RETRY, max_retry_time: wait }
}

async fn read_default_server_id(path: &Path) -> ChordResult<String> {
    let value = tokio::fs::read_to_string(path).await.map_err(ChordError::other)?;
    let value = value.trim().to_string();
    if !pi_protocol::is_server_id(&value) {
        let message = format!("Invalid default experimental server identity in {}", path.display());
        return Err(ChordError::error(message));
    }
    Ok(value)
}

/// Lock one logical server ID in a shared experimental server directory.
pub async fn acquire_server_profile(directory: &Path, requested_server_id: Option<&str>) -> ChordResult<ServerProfile> {
    tokio::fs::create_dir_all(directory).await.map_err(ChordError::other)?;
    let server_id = match requested_server_id {
        Some(id) if !pi_protocol::is_server_id(id) => {
            return Err(ChordError::error(format!("Invalid experimental server ID: {id}")));
        }
        Some(id) => id.to_string(),
        None => {
            let path = directory.join(DEFAULT_SERVER_ID_FILE);
            match tokio::fs::try_exists(&path).await {
                Ok(true) => read_default_server_id(&path).await?,
                _ => {
                    let candidate = pi_server::random_uuid();
                    let mut options = std::fs::OpenOptions::new();
                    options.write(true).create_new(true);
                    #[cfg(unix)]
                    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
                    match options.open(&path) {
                        Ok(mut file) => {
                            std::io::Write::write_all(&mut file, candidate.as_bytes()).map_err(ChordError::other)?;
                            candidate
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                            read_default_server_id(&path).await?
                        }
                        Err(error) => return Err(ChordError::other(error)),
                    }
                }
            }
        }
    };
    let options = lock_options(LOCK_STALE, LOCK_STALE / 3, LOCK_WAIT);
    let lock = lock_file(&directory.join(format!("launcher-{server_id}")), options).await?;
    Ok(ServerProfile { server_id, lock })
}

/// Serialize automatic activation of one logical server.
pub async fn acquire_server_activation(directory: &Path, server_id: &str) -> ChordResult<LockRelease> {
    let options = lock_options(ACTIVATION_TIMEOUT * 2, ACTIVATION_TIMEOUT, ACTIVATION_TIMEOUT);
    lock_file(&directory.join(format!("activation-{server_id}")), options).await
}

#[derive(Default)]
struct LifetimeState {
    connection_count: usize,
    worker_count: usize,
    startup_held: bool,
    startup_timer: Option<JoinHandle<()>>,
    retirement_timer: Option<JoinHandle<()>>,
    retire: Option<Arc<dyn Fn() + Send + Sync>>,
    stopped: bool,
}

/// Reconciles operator, startup, client, and worker holds for one server generation.
#[derive(Clone)]
pub struct ServerLifetime(Arc<(bool, Mutex<LifetimeState>)>);

impl ServerLifetime {
    pub fn new(keep_alive: bool) -> Self {
        Self(Arc::new((keep_alive, Mutex::new(LifetimeState { startup_held: !keep_alive, ..Default::default() }))))
    }

    pub fn start(&self, retire: Arc<dyn Fn() + Send + Sync>) {
        let mut state = lock(&self.0.1);
        state.retire = Some(retire);
        if state.startup_held {
            let weak = Arc::downgrade(&self.0);
            state.startup_timer = Some(tokio::spawn(async move {
                tokio::time::sleep(AUTO_SERVER_STARTUP_GRACE).await;
                let Some(inner) = weak.upgrade() else { return };
                {
                    let mut state = lock(&inner.1);
                    state.startup_timer = None;
                    state.startup_held = false;
                }
                ServerLifetime(inner).reconcile();
            }));
        }
        drop(state);
        self.reconcile();
    }

    pub fn set_connection_count(&self, count: usize) {
        {
            let mut state = lock(&self.0.1);
            state.connection_count = count;
            if count > 0 && state.startup_held {
                state.startup_held = false;
                if let Some(timer) = state.startup_timer.take() {
                    timer.abort();
                }
            }
        }
        self.reconcile();
    }

    pub fn set_worker_count(&self, count: usize) {
        lock(&self.0.1).worker_count = count;
        self.reconcile();
    }

    pub fn stop(&self) {
        let mut state = lock(&self.0.1);
        state.stopped = true;
        for timer in [state.startup_timer.take(), state.retirement_timer.take()].into_iter().flatten() {
            timer.abort();
        }
    }

    fn idle(&self, state: &LifetimeState) -> bool {
        !state.stopped && !self.0.0 && !state.startup_held && state.connection_count == 0 && state.worker_count == 0
    }

    fn reconcile(&self) {
        let mut state = lock(&self.0.1);
        if !self.idle(&state) {
            if let Some(timer) = state.retirement_timer.take() {
                timer.abort();
            }
            return;
        }
        let Some(retire) = state.retire.clone() else { return };
        if state.retirement_timer.is_some() {
            return;
        }
        let weak = Arc::downgrade(&self.0);
        state.retirement_timer = Some(tokio::spawn(async move {
            tokio::time::sleep(AUTO_SERVER_IDLE_GRACE).await;
            let Some(inner) = weak.upgrade() else { return };
            let idle = {
                let mut state = lock(&inner.1);
                state.retirement_timer = None;
                ServerLifetime(inner.clone()).idle(&state)
            };
            if idle {
                retire();
            }
        }));
    }
}
