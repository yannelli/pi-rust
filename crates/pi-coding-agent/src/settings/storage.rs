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

impl SettingsStorage for FileSettingsStorage {
    /// Locks only when the file exists or is written, so reading never creates `.pi/`.
    fn with_lock(&self, scope: SettingsScope, edit: SettingsEdit<'_>) -> Result<(), String> {
        let path = self.file(scope);
        let lock_error = |error: std::io::Error| format!("cannot lock {}: {error}", path.display());
        let mut lock = None;
        let current = if path.exists() {
            lock = Some(FileLock::acquire_blocking(path).map_err(lock_error)?);
            Some(std::fs::read_to_string(path).map_err(|error| error.to_string())?)
        } else {
            None
        };
        if let Some(next) = edit(current.as_deref())? {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            if lock.is_none() {
                lock = Some(FileLock::acquire_blocking(path).map_err(lock_error)?);
            }
            std::fs::write(path, next).map_err(|error| error.to_string())?;
        }
        drop(lock);
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
