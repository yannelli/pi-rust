//! Settings filters for resource paths: plain globs include, `!glob` excludes, `+path`
//! force-includes, and `-path` force-excludes. Ported from upstream `core/package-manager.ts`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use globset::GlobBuilder;

use super::paths::{normalize_path_input, relative_posix, to_posix};

/// True for filter entries (`!`, `+`, `-` prefixes or glob characters) rather than paths.
pub fn is_pattern(entry: &str) -> bool {
    entry.starts_with(['!', '+', '-']) || entry.contains('*') || entry.contains('?')
}

fn posix_pattern(pattern: &str) -> String {
    if cfg!(windows) { pattern.replace('\\', "/") } else { pattern.to_string() }
}

fn glob_matches(pattern: &str, candidate: &str) -> bool {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .is_ok_and(|glob| glob.compile_matcher().is_match(candidate))
}

/// The forms a pattern may name: the path relative to `base`, the file name, and the
/// absolute path, plus the same three for the directory of a `SKILL.md`.
fn candidates(path: &Path, base: &Path) -> (Vec<String>, Vec<String>) {
    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let mut matched = vec![relative_posix(base, path), name.clone(), to_posix(path)];
    let mut exact = vec![relative_posix(base, path), to_posix(path)];
    if name == "SKILL.md"
        && let Some(parent) = path.parent()
    {
        let parent_name = parent.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
        matched.extend([relative_posix(base, parent), parent_name, to_posix(parent)]);
        exact.extend([relative_posix(base, parent), to_posix(parent)]);
    }
    (matched, exact)
}

pub fn matches_any_pattern(path: &Path, patterns: &[&str], base: &Path) -> bool {
    let (matched, _) = candidates(path, base);
    patterns.iter().any(|pattern| {
        let pattern = posix_pattern(&normalize_path_input(pattern));
        matched.iter().any(|candidate| glob_matches(&pattern, candidate))
    })
}

pub fn matches_any_exact_pattern(path: &Path, patterns: &[&str], base: &Path) -> bool {
    if patterns.is_empty() {
        return false;
    }
    let (_, exact) = candidates(path, base);
    patterns.iter().any(|pattern| {
        let stripped = pattern.strip_prefix("./").or_else(|| pattern.strip_prefix(".\\")).unwrap_or(pattern);
        let pattern = posix_pattern(&normalize_path_input(stripped));
        exact.contains(&pattern)
    })
}

fn split_overrides(patterns: &[String]) -> (Vec<&str>, Vec<&str>, Vec<&str>, Vec<&str>) {
    let (mut includes, mut excludes, mut force_includes, mut force_excludes) = (vec![], vec![], vec![], vec![]);
    for pattern in patterns {
        if let Some(rest) = pattern.strip_prefix('+') {
            force_includes.push(rest);
        } else if let Some(rest) = pattern.strip_prefix('-') {
            force_excludes.push(rest);
        } else if let Some(rest) = pattern.strip_prefix('!') {
            excludes.push(rest);
        } else {
            includes.push(pattern.as_str());
        }
    }
    (includes, excludes, force_includes, force_excludes)
}

/// Whether an auto-discovered path stays enabled under the `!`, `+`, and `-` entries.
pub fn is_enabled_by_overrides(path: &Path, patterns: &[String], base: &Path) -> bool {
    let (_, excludes, force_includes, force_excludes) = split_overrides(patterns);
    let mut enabled = excludes.is_empty() || !matches_any_pattern(path, &excludes, base);
    if matches_any_exact_pattern(path, &force_includes, base) {
        enabled = true;
    }
    if matches_any_exact_pattern(path, &force_excludes, base) {
        enabled = false;
    }
    enabled
}

/// The enabled subset of `paths`: plain globs (or everything), minus `!` matches, plus
/// exact `+` paths, minus exact `-` paths.
pub fn apply_patterns(paths: &[PathBuf], patterns: &[String], base: &Path) -> HashSet<PathBuf> {
    let (includes, excludes, force_includes, force_excludes) = split_overrides(patterns);
    let mut result: HashSet<PathBuf> = paths
        .iter()
        .filter(|path| includes.is_empty() || matches_any_pattern(path, &includes, base))
        .filter(|path| excludes.is_empty() || !matches_any_pattern(path, &excludes, base))
        .cloned()
        .collect();
    result.extend(paths.iter().filter(|path| matches_any_exact_pattern(path, &force_includes, base)).cloned());
    result.retain(|path| !matches_any_exact_pattern(path, &force_excludes, base));
    result
}

/// The last of `patterns` naming `path` decides: `+` or plain enables, `-` or `!` disables.
pub fn autoload_override(path: &Path, patterns: &[String], base: &Path) -> Option<bool> {
    let mut decision = None;
    for pattern in patterns {
        let target = pattern.strip_prefix(['+', '-', '!']).unwrap_or(pattern);
        let exact = pattern.starts_with(['+', '-']);
        let matched = if exact {
            matches_any_exact_pattern(path, &[target], base)
        } else {
            matches_any_pattern(path, &[target], base)
        };
        if matched {
            decision = Some(!pattern.starts_with(['-', '!']));
        }
    }
    decision
}
