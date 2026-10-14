//! Transpile results cached by content hash, in memory and optionally on disk.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::{TranspileOptions, Transpiled, transpile};
use crate::error::JsError;

/// Bumped when the cached format or the transform changes.
const CACHE_VERSION: &str = concat!("pi-js-transpile-2-", env!("CARGO_PKG_VERSION"), "-oxc-0.153");

/// Thread-safe transpile cache. Entries are keyed by the SHA-256 of the source text, the file
/// extension, and the options, so a renamed file with the same content reuses its entry.
#[derive(Debug, Default)]
pub struct TranspileCache {
    options: TranspileOptions,
    dir: Option<PathBuf>,
    memory: Mutex<HashMap<String, Arc<Transpiled>>>,
}

impl TranspileCache {
    /// A cache kept in memory, and also in `dir` when one is given.
    pub fn new(options: TranspileOptions, dir: Option<PathBuf>) -> Self {
        Self { options, dir, memory: Mutex::new(HashMap::new()) }
    }

    pub fn options(&self) -> &TranspileOptions {
        &self.options
    }

    /// The cached result for this source, transpiling it on a miss.
    pub fn get_or_transpile(&self, path: &Path, source: &str) -> Result<Arc<Transpiled>, JsError> {
        let key = self.key(path, source);
        if let Some(hit) = self.memory.lock().unwrap_or_else(|poison| poison.into_inner()).get(&key) {
            return Ok(hit.clone());
        }
        let result = match self.read_disk(&key) {
            Some(cached) => cached,
            None => {
                let fresh = transpile(path, source, &self.options)?;
                self.write_disk(&key, &fresh);
                fresh
            }
        };
        let result = Arc::new(result);
        self.memory.lock().unwrap_or_else(|poison| poison.into_inner()).insert(key, result.clone());
        Ok(result)
    }

    /// Number of entries held in memory.
    pub fn len(&self) -> usize {
        self.memory.lock().unwrap_or_else(|poison| poison.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn key(&self, path: &Path, source: &str) -> String {
        let extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or_default();
        let mut context = ring::digest::Context::new(&ring::digest::SHA256);
        for part in [CACHE_VERSION, &self.options.cache_key(), extension] {
            context.update(part.as_bytes());
            context.update(&[0]);
        }
        context.update(source.as_bytes());
        context.finish().as_ref().iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn read_disk(&self, key: &str) -> Option<Transpiled> {
        let text = fs::read_to_string(self.dir.as_ref()?.join(format!("{key}.json"))).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn write_disk(&self, key: &str, value: &Transpiled) {
        let Some(dir) = &self.dir else { return };
        let Ok(text) = serde_json::to_string(value) else { return };
        if fs::create_dir_all(dir).is_err() {
            return;
        }
        // Write then rename so a concurrent reader never sees a partial entry.
        let temp = dir.join(format!("{key}.{}.tmp", std::process::id()));
        if fs::write(&temp, text).is_ok() && fs::rename(&temp, dir.join(format!("{key}.json"))).is_err() {
            let _ = fs::remove_file(&temp);
        }
    }
}
