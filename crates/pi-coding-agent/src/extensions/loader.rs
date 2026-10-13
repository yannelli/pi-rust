//! Extension entry points (upstream `loader.ts`): files load as they are, directories through
//! `package.json` `pi.extensions` or `index.ts`/`index.js`, and `builtin:` paths stay native.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::resources::{ExtensionEntry, SourceInfo};

/// One extension file to import.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadEntry {
    /// The path as configured, reported in errors and `extensionPath`.
    pub path: String,
    pub resolved_path: PathBuf,
    pub source_info: Option<SourceInfo>,
}

impl LoadEntry {
    pub fn file(path: impl Into<PathBuf>) -> Self {
        let resolved_path = path.into();
        Self { path: resolved_path.display().to_string(), resolved_path, source_info: None }
    }

    pub(crate) fn to_json(&self) -> Value {
        let source_info = self.source_info.as_ref().map(|info| json!(info)).unwrap_or_else(|| {
            let base_dir = self.resolved_path.parent().map(|dir| dir.display().to_string());
            json!({"path": self.path, "source": "local", "scope": "temporary", "origin": "top-level", "baseDir": base_dir})
        });
        json!({
            "path": self.path,
            "resolvedPath": self.resolved_path.display().to_string(),
            "sourceInfo": source_info,
        })
    }
}

fn is_extension_file(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "ts" || extension == "js")
}

/// Entry points a directory declares: `package.json` `pi.extensions`, else `index.ts`/`index.js`.
pub fn resolve_extension_entries(dir: &Path) -> Option<Vec<PathBuf>> {
    let manifest = std::fs::read_to_string(dir.join("package.json")).ok();
    let manifest = manifest.and_then(|text| serde_json::from_str::<Value>(&text).ok());
    if let Some(declared) = manifest.as_ref().and_then(|manifest| manifest["pi"]["extensions"].as_array()) {
        let entries: Vec<PathBuf> = declared
            .iter()
            .filter_map(Value::as_str)
            .map(|entry| dir.join(entry))
            .filter(|path| path.exists())
            .collect();
        if !entries.is_empty() {
            return Some(entries);
        }
    }
    ["index.ts", "index.js"].iter().map(|name| dir.join(name)).find(|path| path.exists()).map(|path| vec![path])
}

/// Extension files in `dir`: direct `.ts`/`.js` files and subdirectories with entry points.
pub fn discover_extensions_in_dir(dir: &Path) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut entries: Vec<PathBuf> = read.filter_map(Result::ok).map(|entry| entry.path()).collect();
    entries.sort();
    let mut found = Vec::new();
    for path in entries {
        if path.is_file() && is_extension_file(&path) {
            found.push(path);
        } else if path.is_dir()
            && let Some(resolved) = resolve_extension_entries(&path)
        {
            found.extend(resolved);
        }
    }
    found
}

/// Files to import for the configured entries, in order, without duplicates or `builtin:` paths.
pub fn load_entries(entries: &[ExtensionEntry]) -> Vec<LoadEntry> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for entry in entries {
        let text = entry.path.to_string_lossy();
        if text.starts_with(crate::resources::source_info::BUILTIN_PATH_PREFIX) {
            continue;
        }
        let files = if entry.path.is_dir() {
            resolve_extension_entries(&entry.path).unwrap_or_else(|| discover_extensions_in_dir(&entry.path))
        } else {
            vec![entry.path.clone()]
        };
        let single = files.len() == 1 && files[0] == entry.path;
        for file in files {
            if !seen.insert(file.clone()) {
                continue;
            }
            let path = if single { entry.path.display().to_string() } else { file.display().to_string() };
            result.push(LoadEntry { path, resolved_path: file, source_info: Some(entry.source_info.clone()) });
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directories_resolve_manifest_index_and_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("pkg/src")).unwrap();
        std::fs::write(root.join("pkg/package.json"), r#"{"pi":{"extensions":["src/a.ts","missing.ts"]}}"#).unwrap();
        std::fs::write(root.join("pkg/src/a.ts"), "").unwrap();
        std::fs::create_dir_all(root.join("indexed")).unwrap();
        std::fs::write(root.join("indexed/index.js"), "").unwrap();
        std::fs::write(root.join("plain.ts"), "").unwrap();
        std::fs::write(root.join("notes.md"), "").unwrap();
        let found = discover_extensions_in_dir(root);
        assert_eq!(found, vec![root.join("indexed/index.js"), root.join("pkg/src/a.ts"), root.join("plain.ts")]);
        assert_eq!(resolve_extension_entries(&root.join("pkg")), Some(vec![root.join("pkg/src/a.ts")]));
    }
}
