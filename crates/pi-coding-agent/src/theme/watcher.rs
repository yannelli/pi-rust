//! Hot reload of the active user theme from `<agent-dir>/themes/<name>.json`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use super::registry::{current_theme_name, load_theme_from_path, set_global, upsert_registered};

/// Edits within this window collapse into one reload.
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(100);

/// Watches one theme file. Dropping it stops the watcher and its reload thread.
pub(crate) struct ThemeWatcher {
    _watcher: RecommendedWatcher,
}

impl std::fmt::Debug for ThemeWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ThemeWatcher")
    }
}

/// Reload the theme when its file changes. Keeps the last good theme while the file is missing or invalid.
fn reload(name: &str, path: &Path, notify_changed: fn()) {
    // Ignore stale events after switching themes.
    if current_theme_name().as_deref() != Some(name) || !path.exists() {
        return;
    }
    // A file being edited can be briefly invalid; the next save reloads it.
    if let Ok(theme) = load_theme_from_path(path, None) {
        let theme = Arc::new(theme);
        upsert_registered(theme.clone());
        set_global(theme);
        notify_changed();
    }
}

/// Start watching `path`'s directory for changes to `path`. `None` when the platform watcher fails.
pub(crate) fn start_theme_watcher(name: String, path: PathBuf, notify_changed: fn()) -> Option<ThemeWatcher> {
    let directory = path.parent()?.to_path_buf();
    let file_name = path.file_name()?.to_owned();
    let (sender, receiver) = mpsc::channel::<()>();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else { return };
        if matches!(event.kind, EventKind::Access(_)) {
            return;
        }
        let relevant = event.paths.is_empty() || event.paths.iter().any(|p| p.file_name() == Some(&file_name));
        if relevant {
            let _ = sender.send(());
        }
    })
    .ok()?;
    watcher.watch(&directory, RecursiveMode::NonRecursive).ok()?;
    std::thread::Builder::new()
        .name("pi-theme-watcher".into())
        .spawn(move || {
            // The sender lives in the watcher's handler, so dropping the watcher ends this loop.
            while receiver.recv().is_ok() {
                loop {
                    match receiver.recv_timeout(RELOAD_DEBOUNCE) {
                        Ok(()) => continue,
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
                reload(&name, &path, notify_changed);
            }
        })
        .ok()?;
    Some(ThemeWatcher { _watcher: watcher })
}
