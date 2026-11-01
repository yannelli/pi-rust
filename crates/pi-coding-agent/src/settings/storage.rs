//! Where settings live: the global and project `settings.json` files, guarded by the same
//! `<file>.lock` directory lock the TypeScript pi uses, or memory for tests and embedders.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use pi_ai::auth::store_lock::FileLock;

/// `global` is `<agent dir>/settings.json`; `project` is `<cwd>/.pi/settings.json`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SettingsScope {
    Global,
    Project,
}

impl SettingsScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
        }
    }
}

/// Text after an edit: `Some` writes it, `None` leaves the file alone.
pub type SettingsEdit<'a> = &'a mut dyn FnMut(Option<&str>) -> Result<Option<String>, String>;

/// Locked read, edit, and write access to the settings of one scope.
pub trait SettingsStorage: Send + Sync {
    fn with_lock(&self, scope: SettingsScope, edit: SettingsEdit<'_>) -> Result<(), String>;

    /// File path of a scope, used in diagnostics.
    fn path(&self, _scope: SettingsScope) -> Option<PathBuf> {
        None
    }
}

/// The settings files of an agent directory and a project.
pub struct FileSettingsStorage {
    global: PathBuf,
    project: PathBuf,
}

impl FileSettingsStorage {
    pub fn new(cwd: &Path, agent_dir: &Path) -> Self {
        Self {
            global: agent_dir.join("settings.json"),
            project: crate::resources::brand::project_config_dir(cwd).join("settings.json"),
        }
    }

    fn file(&self, scope: SettingsScope) -> &Path {
        match scope {
            SettingsScope::Global => &self.global,
            SettingsScope::Project => &self.project,
        }
    }
}

/// Node's message for a failed file call, such as `EISDIR: illegal operation on a directory, read`.
fn node_error(error: &std::io::Error, syscall: &str, path: &Path) -> String {
    let path = path.display().to_string();
    let path = (error.kind() != std::io::ErrorKind::IsADirectory).then_some(path.as_str());
    pi_js::JsError::from_io(error, if path.is_some() { syscall } else { "read" }, path).message
}

/// Upstream's lock: 10 tries 20 ms apart, then proper-lockfile's `ELOCKED` message.
fn lock(path: &Path) -> Result<FileLock, String> {
    FileLock::acquire_blocking(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::WouldBlock => "Lock file is already being held".to_string(),
        _ => node_error(&error, "mkdir", &pi_ai::auth::store_lock::lock_dir(path)),
    })
}

impl SettingsStorage for FileSettingsStorage {
    /// Locks only when the file exists or is written, so reading never creates `.pi/`.
    fn with_lock(&self, scope: SettingsScope, edit: SettingsEdit<'_>) -> Result<(), String> {
        let path = self.file(scope);
        let mut held = None;
        let current = if path.exists() {
            held = Some(lock(path)?);
            let bytes = std::fs::read(path).map_err(|error| node_error(&error, "open", path))?;
            Some(String::from_utf8_lossy(&bytes).into_owned())
        } else {
            None
        };
        if let Some(next) = edit(current.as_deref())? {
            if let Some(parent) = path.parent().filter(|parent| !parent.exists()) {
                std::fs::create_dir_all(parent).map_err(|error| node_error(&error, "mkdir", parent))?;
            }
            if held.is_none() {
                held = Some(lock(path)?);
            }
            std::fs::write(path, next).map_err(|error| node_error(&error, "open", path))?;
        }
        drop(held);
        Ok(())
    }

    fn path(&self, scope: SettingsScope) -> Option<PathBuf> {
        Some(self.file(scope).to_path_buf())
    }
}

/// Settings kept in memory.
#[derive(Default)]
pub struct InMemorySettingsStorage {
    global: Mutex<Option<String>>,
    project: Mutex<Option<String>>,
}

impl SettingsStorage for InMemorySettingsStorage {
    fn with_lock(&self, scope: SettingsScope, edit: SettingsEdit<'_>) -> Result<(), String> {
        let slot = match scope {
            SettingsScope::Global => &self.global,
            SettingsScope::Project => &self.project,
        };
        let mut slot = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(next) = edit(slot.as_deref())? {
            *slot = Some(next);
        }
        Ok(())
    }
}
