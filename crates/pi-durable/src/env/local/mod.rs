//! `LocalExecutionEnv`: the execution environment on this machine's file system and processes (upstream
//! `NodeExecutionEnv`).

mod fs;
mod paths;
mod process;
mod readers;
mod shell;
mod spill;
mod watch;
mod watch_scan;

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};

pub use watch::LocalFileWatcher;

/// How a `LocalExecutionEnv` watches.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LocalWatchOptions {
    /// Force a mode. By default `polling` is chosen on Windows, where native watchers keep directories open and so
    /// block renaming their parents, and for file systems that do not report remote changes.
    pub mode: Option<crate::env::WatchMode>,
    /// Interval between snapshots in `polling` mode; default 2000 ms.
    pub poll_interval_ms: Option<u64>,
    /// Most directories one watcher covers; default 10,000.
    pub max_directories: Option<usize>,
}

/// Options of `LocalExecutionEnv::new`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LocalEnvOptions {
    /// The working directory relative paths resolve against.
    pub cwd: String,
    /// Bash to run string commands with; by default `/bin/bash`, `bash` on `PATH`, or `sh` (Git Bash on Windows).
    pub shell_path: Option<String>,
    /// Variables every command gets on top of this process's environment.
    pub shell_env: Option<BTreeMap<String, String>>,
    pub watch: LocalWatchOptions,
    /// Directory of temporary files and spills; default the system temporary directory. Not in upstream.
    pub temp_dir: Option<String>,
}

impl LocalEnvOptions {
    pub fn new(cwd: impl Into<String>) -> Self {
        Self { cwd: cwd.into(), ..Self::default() }
    }
}

/// The local execution environment. File operations and commands run on this machine.
pub struct LocalExecutionEnv {
    cwd: RwLock<String>,
    shell_path: Option<String>,
    shell_env: Option<BTreeMap<String, String>>,
    watch_options: LocalWatchOptions,
    temp_dir: Option<String>,
    active_pids: Arc<Mutex<HashSet<u32>>>,
}

/// The file namespace id every local environment shares.
pub const LOCAL_ENV_ID: &str = "node:local";

impl LocalExecutionEnv {
    pub fn new(options: LocalEnvOptions) -> Self {
        Self {
            cwd: RwLock::new(options.cwd),
            shell_path: options.shell_path,
            shell_env: options.shell_env,
            watch_options: options.watch,
            temp_dir: options.temp_dir,
            active_pids: Arc::default(),
        }
    }

    /// Change the working directory relative paths resolve against.
    pub fn set_cwd(&self, cwd: impl Into<String>) {
        *self.cwd.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = cwd.into();
    }

    fn current_cwd(&self) -> String {
        self.cwd.read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    fn resolve(&self, path: &str) -> String {
        paths::resolve_path(&self.current_cwd(), path)
    }
}

impl std::fmt::Debug for LocalExecutionEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalExecutionEnv")
            .field("cwd", &self.current_cwd())
            .field("shell_path", &self.shell_path)
            .field("watch", &self.watch_options)
            .finish()
    }
}
