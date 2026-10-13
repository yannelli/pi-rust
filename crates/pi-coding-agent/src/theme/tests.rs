//! Theme tests, ported from upstream `coding-agent/test/*theme*.test.ts`.

mod builtin;
mod controller;
mod schema;
mod style;
mod system;
mod watcher;

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use pi_tui::colors::TerminalColorMode;
use serde_json::Value;

use super::*;

static LOCK: Mutex<()> = Mutex::new(());

/// Serializes tests that touch the process-wide theme and terminal state, and resets that state.
pub(crate) struct GlobalState {
    _guard: MutexGuard<'static, ()>,
}

pub(crate) fn global_state() -> GlobalState {
    let guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    reset_globals();
    GlobalState { _guard: guard }
}

fn reset_globals() {
    set_terminal_colors(Default::default());
    set_terminal_color_scheme(None);
    set_theme_environment(Some(std::sync::Arc::new(pi_tui::env::MapEnv::default())));
    set_custom_themes_dir(None);
    let _ = set_registered_themes(Vec::new());
    stop_theme_watcher();
    init_theme(Some("dark"), false);
}

impl Drop for GlobalState {
    fn drop(&mut self) {
        reset_globals();
    }
}

/// The built-in theme document as JSON, for tests that edit a copy.
pub(super) fn builtin_json(name: &str) -> Value {
    serde_json::from_str(builtin_theme_source(name).expect("built-in theme")).expect("valid JSON")
}

/// Write a theme document to `<dir>/<name>.json` and return the path.
pub(super) fn write_theme(dir: &std::path::Path, json: &Value) -> PathBuf {
    let name = json["name"].as_str().unwrap_or("theme");
    let path = dir.join(format!("{name}.json"));
    std::fs::write(&path, serde_json::to_string(json).expect("serialize")).expect("write theme");
    path
}

/// Load a copy of a built-in theme, changed by `edit`, in truecolor.
pub(super) fn load_edited(base: &str, edit: impl FnOnce(&mut Value)) -> Theme {
    let mut json = builtin_json(base);
    edit(&mut json);
    let dir = tempfile::tempdir().expect("temp dir");
    let path = write_theme(dir.path(), &json);
    load_theme_from_path(&path, Some(TerminalColorMode::Truecolor)).expect("theme loads")
}
