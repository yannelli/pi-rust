//! Path resolution shared by the file tools.

use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use unicode_normalization::UnicodeNormalization;

const NARROW_NO_BREAK_SPACE: char = '\u{202F}';

fn is_unicode_space(c: char) -> bool {
    matches!(c, '\u{00A0}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}')
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}

/// Normalize unicode spaces, strip a leading `@`, expand `~`, and decode `file://` URLs.
pub fn expand_path(input: &str) -> PathBuf {
    let mut normalized: String = input.chars().map(|c| if is_unicode_space(c) { ' ' } else { c }).collect();
    if let Some(rest) = normalized.strip_prefix('@') {
        normalized = rest.to_string();
    }
    if let Some(home) = home_dir() {
        if normalized == "~" {
            return home;
        }
        if let Some(rest) = normalized.strip_prefix("~/") {
            return home.join(rest);
        }
    }
    if let Some(rest) = normalized.strip_prefix("file://") {
        return PathBuf::from(percent_decode(rest));
    }
    PathBuf::from(normalized)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| (b as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Lexically resolve `.` and `..` components, like Node's `path.resolve`.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Resolve a path relative to `cwd`, handling `~` expansion and absolute paths.
pub fn resolve_to_cwd(input: &str, cwd: &Path) -> PathBuf {
    let expanded = expand_path(input);
    if expanded.is_absolute() { normalize_lexically(&expanded) } else { normalize_lexically(&cwd.join(expanded)) }
}

static AM_PM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i) (AM|PM)\.").expect("valid regex"));

/// Like [`resolve_to_cwd`], but tries macOS screenshot filename variants when the path is missing.
pub fn resolve_read_path(input: &str, cwd: &Path) -> PathBuf {
    let resolved = resolve_to_cwd(input, cwd);
    if resolved.exists() {
        return resolved;
    }
    let Some(text) = resolved.to_str() else {
        return resolved;
    };
    let am_pm = AM_PM.replace_all(text, format!("{NARROW_NO_BREAK_SPACE}$1.").as_str()).into_owned();
    let nfd: String = text.nfd().collect();
    let curly = text.replace('\'', "\u{2019}");
    let nfd_curly = nfd.replace('\'', "\u{2019}");
    for candidate in [am_pm, nfd, curly, nfd_curly] {
        if candidate != text && Path::new(&candidate).exists() {
            return PathBuf::from(candidate);
        }
    }
    resolved
}
