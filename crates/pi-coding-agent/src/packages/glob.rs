//! Glob entries of a `pi` manifest, like Node `fs.globSync` with the upstream filter:
//! matches are sorted, dot-prefixed segments are dropped, and symlinked directories are
//! not traversed (list such roots as exact entries).

use std::path::{Path, PathBuf};

use globset::GlobBuilder;

use crate::resources::paths::{normalize, relative_posix, sorted_entries};

fn is_dynamic(segment: &str) -> bool {
    segment.contains(['*', '?', '[', '{'])
}

fn walk(dir: &Path, prefix: &str, depth: usize, max_depth: Option<usize>, out: &mut Vec<(String, PathBuf, bool)>) {
    if max_depth.is_some_and(|max| depth >= max) {
        return;
    }
    for (name, path) in sorted_entries(dir) {
        if name.starts_with('.') {
            continue;
        }
        let relative = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
        let Ok(link) = std::fs::symlink_metadata(&path) else { continue };
        let is_dir = std::fs::metadata(&path).is_ok_and(|meta| meta.is_dir());
        out.push((relative.clone(), path.clone(), is_dir));
        if link.is_dir() {
            walk(&path, &relative, depth + 1, max_depth, out);
        }
    }
}

/// Paths under `root` matching `pattern`, sorted, without dot-prefixed segments.
pub(crate) fn expand_package_glob(pattern: &str, root: &Path) -> Vec<PathBuf> {
    let pattern = if cfg!(windows) { pattern.replace('\\', "/") } else { pattern.to_string() };
    let dirs_only = pattern.ends_with('/');
    let segments: Vec<&str> = pattern.trim_end_matches('/').split('/').collect();
    let static_count = segments.iter().take_while(|segment| !is_dynamic(segment)).count();
    if static_count == segments.len() {
        let path = normalize(&root.join(segments.join("/")));
        return if path.exists() { vec![path] } else { Vec::new() };
    }
    let base_text = segments[..static_count].join("/");
    let base =
        if base_text.is_empty() && pattern.starts_with('/') { PathBuf::from("/") } else { root.join(&base_text) };
    let rest = segments[static_count..].join("/");
    let Ok(glob) = GlobBuilder::new(&rest).literal_separator(true).build() else { return Vec::new() };
    let matcher = glob.compile_matcher();
    let max_depth = (!rest.contains("**")).then(|| segments.len() - static_count);
    let mut entries = Vec::new();
    walk(&base, "", 0, max_depth, &mut entries);
    let mut matches: Vec<PathBuf> = entries
        .into_iter()
        .filter(|(relative, _, is_dir)| (!dirs_only || *is_dir) && matcher.is_match(relative))
        .map(|(_, path, _)| normalize(&path))
        .filter(|path| {
            relative_posix(root, path).split('/').all(|segment| segment == ".." || !segment.starts_with('.'))
        })
        .collect();
    matches.sort_by(|a, b| a.as_os_str().cmp(b.as_os_str()));
    matches.dedup();
    matches
}
