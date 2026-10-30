//! Extension entry points (upstream `loader.ts`): files load as they are, directories through
//! `package.json` `pi.extensions` or `index.ts`/`index.js`, and `builtin:` paths stay native.
//! `pi.native` programs and executable files load through the native bridge (`js/native.js`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::packages::manifest::{NATIVE_MANIFEST_ENTRY, NativeCommand, native_commands};
use crate::resources::{ExtensionEntry, SourceInfo};

/// One extension file to import.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadEntry {
    /// The path as configured, reported in errors and `extensionPath`.
    pub path: String,
    pub resolved_path: PathBuf,
    pub source_info: Option<SourceInfo>,
    /// `{ command, args, env }` of a program the native bridge starts in place of an import.
    pub native: Option<Value>,
}

impl LoadEntry {
    pub fn file(path: impl Into<PathBuf>) -> Self {
        let resolved_path = path.into();
        Self { path: resolved_path.display().to_string(), resolved_path, source_info: None, native: None }
    }

    fn native(command: NativeCommand, source_info: Option<SourceInfo>) -> Self {
        let native =
            json!({"command": command.program.display().to_string(), "args": command.args, "env": command.env});
        Self { path: command.extension_path, resolved_path: command.program, source_info, native: Some(native) }
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
            "native": self.native,
        })
    }
}

fn is_extension_file(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "ts" || extension == "js")
}

/// Entry points a directory declares: `package.json` `pi.extensions` and `pi.native`, else
/// `index.ts`/`index.js`.
pub fn resolve_extension_entries(dir: &Path) -> Option<Vec<PathBuf>> {
    crate::resources::discovery::resolve_extension_entries(dir)
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata().is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
}

/// The program of a `native:` path the package manager resolved, from its package's manifest.
fn packaged_native_command(path: &str, source_info: &SourceInfo) -> Option<NativeCommand> {
    let root = PathBuf::from(source_info.base_dir.as_ref()?);
    native_commands(&root.join(NATIVE_MANIFEST_ENTRY)).into_iter().find(|command| command.extension_path == path)
}

/// The native programs `file` stands for: the `pi.native` entries of a `package.json`, or the
/// file itself when it is an executable that is not JavaScript or TypeScript.
fn native_commands_of(file: &Path) -> Option<Vec<NativeCommand>> {
    if file.file_name().is_some_and(|name| name == NATIVE_MANIFEST_ENTRY) {
        return Some(native_commands(file));
    }
    let known = file
        .extension()
        .is_some_and(|extension| ["ts", "js", "mjs", "cjs", "mts", "cts"].contains(&&*extension.to_string_lossy()));
    if known || !file.is_file() || !is_executable(file) {
        return None;
    }
    let name = file.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default();
    let command = NativeCommand {
        extension_path: format!("native:{name}"),
        program: file.to_path_buf(),
        args: Vec::new(),
        env: Default::default(),
    };
    Some(vec![command])
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
        if text.starts_with(crate::resources::source_info::NATIVE_PATH_PREFIX) {
            if let Some(command) = packaged_native_command(&text, &entry.source_info)
                && seen.insert(entry.path.clone())
            {
                result.push(LoadEntry::native(command, Some(entry.source_info.clone())));
            }
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
            if let Some(commands) = native_commands_of(&file) {
                let source_info = Some(entry.source_info.clone());
                result.extend(commands.into_iter().map(|command| LoadEntry::native(command, source_info.clone())));
                continue;
            }
            let path = if single { entry.path.display().to_string() } else { file.display().to_string() };
            let source_info = Some(entry.source_info.clone());
            result.push(LoadEntry { path, resolved_path: file, source_info, native: None });
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

    fn extension_entry(path: PathBuf) -> ExtensionEntry {
        use crate::resources::source_info::{PathMetadata, SourceOrigin, SourceScope, create_source_info};
        let metadata = PathMetadata::new("local", SourceScope::Temporary, SourceOrigin::TopLevel);
        ExtensionEntry { source_info: create_source_info(&path.display().to_string(), &metadata), path }
    }

    #[test]
    fn native_entries_resolve_against_the_package() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("todo");
        std::fs::create_dir_all(&pkg).unwrap();
        let manifest = r#"{"name":"@x/todo","pi":{"extensions":["a.ts"],"native":[
            {"command":"bin/ext","args":["todo"],"env":{"K":"v"}},{"command":"/abs/tool"},{"args":["no-command"]}]}}"#;
        std::fs::write(pkg.join("package.json"), manifest).unwrap();
        std::fs::write(pkg.join("a.ts"), "").unwrap();
        let entries = load_entries(&[extension_entry(pkg.clone())]);
        let paths: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![pkg.join("a.ts").display().to_string().as_str(), "native:@x/todo/todo", "native:@x/todo/tool"]
        );
        let program = crate::packages::manifest::native_program(&pkg, "bin/ext");
        assert_eq!(entries[1].resolved_path, program);
        let native = entries[1].to_json()["native"].clone();
        assert_eq!(native, json!({"command": program.display().to_string(), "args": ["todo"], "env": {"K": "v"}}));
        assert_eq!(entries[0].to_json()["native"], Value::Null);
    }

    #[cfg(unix)]
    #[test]
    fn executable_files_load_as_native_extensions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("hello-ext");
        std::fs::write(&binary, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        let script = dir.path().join("plain.js");
        std::fs::write(&script, "").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let entries = load_entries(&[extension_entry(binary.clone()), extension_entry(script.clone())]);
        assert_eq!(entries[0].path, "native:hello-ext");
        assert_eq!(entries[0].native.as_ref().unwrap()["command"], binary.display().to_string());
        assert!(entries[1].native.is_none());
    }
}
