//! Direct path completion: lists the directory named by the prefix (upstream `getFileSuggestions`).

use std::fs;
use std::path::Path;

use super::AutocompleteItem;
use super::paths::{
    basename, build_completion_value, dirname, join, locale_compare, parse_path_prefix, to_display_path,
};

/// The user's home directory as a string.
pub(crate) fn home_dir() -> String {
    #[allow(deprecated)]
    let home = std::env::home_dir();
    home.map(|path| path.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Expands `~` and `~/...` to the home directory, keeping a trailing slash.
pub(crate) fn expand_home_path(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        let expanded = join(&[&home_dir(), rest]);
        if path.ends_with('/') && !expanded.ends_with('/') { format!("{expanded}/") } else { expanded }
    } else if path == "~" {
        home_dir()
    } else {
        path.to_string()
    }
}

fn is_absolute_like(raw: &str, expanded: &str) -> bool {
    raw.starts_with('~') || expanded.starts_with('/') || Path::new(expanded).is_absolute()
}

/// Completes `prefix` against the entries of its directory: directories first, then by name.
pub(crate) fn get_file_suggestions(base_path: &str, prefix: &str) -> Vec<AutocompleteItem> {
    let parsed = parse_path_prefix(prefix);
    let raw = parsed.raw;
    let expanded = if raw.starts_with('~') { expand_home_path(raw) } else { raw.to_string() };

    let is_root_prefix = matches!(raw, "" | "./" | "../" | "~" | "~/" | "/");
    let (search_dir, search_prefix) = if is_root_prefix || raw.ends_with('/') {
        let dir = if is_absolute_like(raw, &expanded) { expanded.clone() } else { join(&[base_path, &expanded]) };
        (dir, String::new())
    } else {
        let dir = dirname(&expanded);
        let file = basename(&expanded);
        let dir = if is_absolute_like(raw, &expanded) { dir } else { join(&[base_path, &dir]) };
        (dir, file)
    };

    let Ok(entries) = fs::read_dir(&search_dir) else {
        return Vec::new();
    };
    let search_lower = search_prefix.to_lowercase();
    let mut suggestions = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.to_lowercase().starts_with(&search_lower) {
            continue;
        }
        // Symlinks to directories count as directories.
        let is_directory = entry.file_type().is_ok_and(|kind| kind.is_dir())
            || (entry.file_type().is_ok_and(|kind| kind.is_symlink())
                && fs::metadata(Path::new(&search_dir).join(&name)).is_ok_and(|meta| meta.is_dir()));
        let relative = to_display_path(&display_path(raw, &name));
        let path_value = if is_directory { format!("{relative}/") } else { relative };
        suggestions.push(AutocompleteItem {
            value: build_completion_value(&path_value, parsed.is_at, parsed.is_quoted),
            label: if is_directory { format!("{name}/") } else { name },
            description: None,
        });
    }
    suggestions.sort_by(|a, b| {
        let a_dir = a.label.ends_with('/');
        let b_dir = b.label.ends_with('/');
        b_dir.cmp(&a_dir).then_with(|| locale_compare(&a.label, &b.label))
    });
    suggestions
}

/// The completed path as the user typed it: keeps `~/`, `./`, and absolute forms.
fn display_path(prefix: &str, name: &str) -> String {
    if prefix.ends_with('/') {
        return format!("{prefix}{name}");
    }
    if prefix.contains('/') || prefix.contains('\\') {
        if let Some(home_relative) = prefix.strip_prefix("~/") {
            let dir = dirname(home_relative);
            return if dir == "." { format!("~/{name}") } else { format!("~/{}", join(&[&dir, name])) };
        }
        if prefix.starts_with('/') {
            let dir = dirname(prefix);
            return if dir == "/" { format!("/{name}") } else { format!("{dir}/{name}") };
        }
        let joined = join(&[&dirname(prefix), name]);
        return if prefix.starts_with("./") && !joined.starts_with("./") { format!("./{joined}") } else { joined };
    }
    if prefix.starts_with('~') { format!("~/{name}") } else { name.to_string() }
}
