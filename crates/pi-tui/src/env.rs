//! Environment variable access that tests can replace.
//! Rust 2024 makes `std::env::set_var` unsafe, so code that reads the environment takes an
//! [`EnvLookup`] and has a wrapper that reads the process environment.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

/// Reads one environment variable. Returns `None` when it is unset or not valid Unicode.
pub trait EnvLookup {
    fn var(&self, name: &str) -> Option<String>;

    /// Returns the variable when it is set and not empty.
    fn non_empty(&self, name: &str) -> Option<String> {
        self.var(name).filter(|value| !value.is_empty())
    }
}

/// The real process environment.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessEnv;

impl EnvLookup for ProcessEnv {
    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

/// A fixed set of variables, for tests and for callers that snapshot the environment.
#[derive(Debug, Clone, Default)]
pub struct MapEnv(pub HashMap<String, String>);

impl MapEnv {
    pub fn new<K: Into<String>, V: Into<String>>(pairs: impl IntoIterator<Item = (K, V)>) -> Self {
        Self(pairs.into_iter().map(|(k, v)| (k.into(), v.into())).collect())
    }
}

impl EnvLookup for MapEnv {
    fn var(&self, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }
}

impl<F: Fn(&str) -> Option<String>> EnvLookup for F {
    fn var(&self, name: &str) -> Option<String> {
        self(name)
    }
}

static GLOBAL_STATE_LOCK: Mutex<()> = Mutex::new(());

/// Serializes tests that touch process-wide state (Kitty protocol flag, capabilities, keybindings).
#[doc(hidden)]
pub fn global_state_lock() -> MutexGuard<'static, ()> {
    GLOBAL_STATE_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
