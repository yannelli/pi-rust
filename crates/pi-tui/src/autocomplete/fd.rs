//! `@` fuzzy file search with `fd` (fast, respects `.gitignore`). The walk runs on a background
//! thread; aborting the request kills the `fd` process.

use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};

use super::paths::{basename, build_completion_value, join, locale_compare, to_display_path};
use super::{AbortSignal, AutocompleteItem, LocalBoxFuture, files::expand_home_path};

const MAX_RESULTS: usize = 100;
const MAX_SUGGESTIONS: usize = 20;

#[derive(Clone, Debug)]
struct Entry {
    path: String,
    is_directory: bool,
}

fn escape_regex(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for c in value.chars() {
        if ".*+?^${}()|[]\\".contains(c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

/// Turns `a/b` into a regex that matches either path separator.
fn build_fd_path_query(query: &str) -> String {
    let normalized = to_display_path(query);
    if !normalized.contains('/') {
        return normalized;
    }
    let has_trailing_separator = normalized.ends_with('/');
    let trimmed = normalized.trim_matches('/');
    if trimmed.is_empty() {
        return normalized;
    }
    let separator = "[\\\\/]";
    let segments: Vec<String> = trimmed.split('/').filter(|s| !s.is_empty()).map(escape_regex).collect();
    if segments.is_empty() {
        return normalized;
    }
    let mut pattern = segments.join(separator);
    if has_trailing_separator {
        pattern.push_str(separator);
    }
    pattern
}

fn walk_directory_with_fd(
    base_dir: &str,
    fd_path: &str,
    query: &str,
    signal: &AbortSignal,
    max_depth: Option<usize>,
) -> Vec<Entry> {
    if signal.is_aborted() {
        return Vec::new();
    }
    let mut args: Vec<String> = ["--base-directory", base_dir, "--max-results", &MAX_RESULTS.to_string()]
        .iter()
        .map(|s| s.to_string())
        .collect();
    for arg in ["--type", "f", "--type", "d", "--follow", "--hidden"] {
        args.push(arg.to_string());
    }
    for exclude in [".git", ".git/*", ".git/**"] {
        args.push("--exclude".to_string());
        args.push(exclude.to_string());
    }
    if let Some(depth) = max_depth {
        args.push("--max-depth".to_string());
        args.push(depth.to_string());
    }
    if to_display_path(query).contains('/') {
        args.push("--full-path".to_string());
    }
    if !query.is_empty() {
        args.push(build_fd_path_query(query));
    }

    let Ok(mut child) =
        Command::new(fd_path).args(&args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()
    else {
        return Vec::new();
    };
    let stdout = child.stdout.take();
    let child: Arc<Mutex<Child>> = Arc::new(Mutex::new(child));
    let killer = Arc::clone(&child);
    signal.on_abort(move || {
        // A held lock means the reader is already reaping the process.
        if let Ok(mut child) = killer.try_lock() {
            let _ = child.kill();
        }
    });
    let mut output = String::new();
    if let Some(mut stdout) = stdout {
        let _ = stdout.read_to_string(&mut output);
    }
    let status = child.lock().unwrap_or_else(PoisonError::into_inner).wait();
    if signal.is_aborted() || !status.is_ok_and(|s| s.success()) || output.is_empty() {
        return Vec::new();
    }
    parse_fd_output(&output)
}

fn parse_fd_output(output: &str) -> Vec<Entry> {
    let mut results = Vec::new();
    for line in output.trim().split('\n').filter(|line| !line.is_empty()) {
        let display = to_display_path(line.trim_end_matches('\r'));
        let is_directory = display.ends_with('/');
        let normalized = display.strip_suffix('/').unwrap_or(&display);
        if normalized == ".git" || normalized.starts_with(".git/") || normalized.contains("/.git/") {
            continue;
        }
        results.push(Entry { path: display.clone(), is_directory });
    }
    results
}

/// Higher is better; directories get a bonus.
fn score_entry(file_path: &str, query: &str, is_directory: bool) -> u32 {
    let file_name = basename(file_path).to_lowercase();
    let query = query.to_lowercase();
    let mut score = if file_name == query {
        100
    } else if file_name.starts_with(&query) {
        80
    } else if file_name.contains(&query) {
        50
    } else if file_path.to_lowercase().contains(&query) {
        30
    } else {
        0
    };
    if is_directory && score > 0 {
        score += 10;
    }
    score
}

struct ScopedQuery {
    base_dir: String,
    query: String,
    display_base: String,
}

/// Splits `dir/partial` into a search directory and a query when `dir` exists.
fn resolve_scoped_query(base_path: &str, raw_query: &str) -> Option<ScopedQuery> {
    let normalized = to_display_path(raw_query);
    let slash = normalized.rfind('/')?;
    let display_base = normalized[..=slash].to_string();
    let query = normalized[slash + 1..].to_string();
    let base_dir = if display_base.starts_with("~/") {
        expand_home_path(&display_base)
    } else if display_base.starts_with('/') {
        display_base.clone()
    } else {
        join(&[base_path, &display_base])
    };
    fs::metadata(&base_dir).ok()?.is_dir().then_some(ScopedQuery { base_dir, query, display_base })
}

fn scoped_path_for_display(display_base: &str, relative: &str) -> String {
    let relative = to_display_path(relative);
    if display_base == "/" { format!("/{relative}") } else { format!("{}{relative}", to_display_path(display_base)) }
}

fn depth(path: &str) -> usize {
    to_display_path(path).split('/').filter(|s| !s.is_empty()).count()
}

fn fuzzy_file_suggestions_blocking(
    fd_path: &str,
    base_path: &str,
    query: &str,
    is_quoted: bool,
    signal: &AbortSignal,
) -> Vec<AutocompleteItem> {
    let scoped = resolve_scoped_query(base_path, query);
    let fd_base_dir = scoped.as_ref().map_or(base_path, |s| s.base_dir.as_str());
    let fd_query = scoped.as_ref().map_or(query, |s| s.query.as_str());

    // Direct children first, so a flood of deep matches cannot push them out.
    let base_entries = walk_directory_with_fd(fd_base_dir, fd_path, fd_query, signal, Some(1));
    let recursive = walk_directory_with_fd(fd_base_dir, fd_path, fd_query, signal, None);
    let mut seen: HashSet<String> = base_entries.iter().map(|e| e.path.clone()).collect();
    let mut entries = base_entries;
    entries.extend(recursive.into_iter().filter(|e| seen.insert(e.path.clone())));
    if signal.is_aborted() {
        return Vec::new();
    }

    let mut scored: Vec<(Entry, u32)> = entries
        .into_iter()
        .map(|e| {
            let score = if fd_query.is_empty() { 1 } else { score_entry(&e.path, fd_query, e.is_directory) };
            (e, score)
        })
        .filter(|(_, score)| *score > 0)
        .collect();
    scored.sort_by(|(a, a_score), (b, b_score)| {
        b_score
            .cmp(a_score)
            .then_with(|| depth(&a.path).cmp(&depth(&b.path)))
            .then_with(|| a.path.encode_utf16().count().cmp(&b.path.encode_utf16().count()))
            .then_with(|| locale_compare(&a.path, &b.path))
    });

    scored
        .into_iter()
        .take(MAX_SUGGESTIONS)
        .map(|(entry, _)| {
            let without_slash = if entry.is_directory { &entry.path[..entry.path.len() - 1] } else { &entry.path[..] };
            let display = match &scoped {
                Some(scoped) => scoped_path_for_display(&scoped.display_base, without_slash),
                None => without_slash.to_string(),
            };
            let name = basename(without_slash);
            let completion = if entry.is_directory { format!("{display}/") } else { display.clone() };
            AutocompleteItem {
                value: build_completion_value(&completion, true, is_quoted),
                label: if entry.is_directory { format!("{name}/") } else { name },
                description: Some(display),
            }
        })
        .collect()
}

/// Runs the `@` search on a background thread. Without `fd` there are no suggestions.
pub(crate) fn fuzzy_file_suggestions(
    fd_path: Option<String>,
    base_path: String,
    query: String,
    is_quoted: bool,
    signal: AbortSignal,
) -> LocalBoxFuture<Vec<AutocompleteItem>> {
    let Some(fd_path) = fd_path.filter(|_| !signal.is_aborted()) else {
        return Box::pin(std::future::ready(Vec::new()));
    };
    let (tx, rx) = tokio::sync::oneshot::channel();
    let spawned = std::thread::Builder::new().name("pi-tui-fd".to_string()).spawn(move || {
        let items = fuzzy_file_suggestions_blocking(&fd_path, &base_path, &query, is_quoted, &signal);
        let _ = tx.send(items);
    });
    if spawned.is_err() {
        return Box::pin(std::future::ready(Vec::new()));
    }
    Box::pin(async move { rx.await.unwrap_or_default() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_separator_agnostic_queries() {
        assert_eq!(build_fd_path_query("src"), "src");
        assert_eq!(build_fd_path_query("tui/src/auto"), "tui[\\\\/]src[\\\\/]auto");
        assert_eq!(build_fd_path_query("components/"), "components[\\\\/]");
        assert_eq!(build_fd_path_query("a.b/c"), "a\\.b[\\\\/]c");
        assert_eq!(build_fd_path_query("/"), "/");
    }

    #[test]
    fn parses_output_and_skips_git() {
        let entries = parse_fd_output("src/\nREADME.md\n.git/\n.git/config\nsub/.git/x\n");
        let paths: Vec<_> = entries.iter().map(|e| (e.path.as_str(), e.is_directory)).collect();
        assert_eq!(paths, [("src/", true), ("README.md", false)]);
    }

    #[test]
    fn scores_entries() {
        assert_eq!(score_entry("src/index.ts", "index.ts", false), 100);
        assert_eq!(score_entry("src/index.ts", "ind", false), 80);
        assert_eq!(score_entry("src/index.ts", "dex", false), 50);
        assert_eq!(score_entry("src/index.ts", "src", false), 30);
        assert_eq!(score_entry("src", "src", true), 110);
        assert_eq!(score_entry("src", "zzz", true), 0);
    }
}
