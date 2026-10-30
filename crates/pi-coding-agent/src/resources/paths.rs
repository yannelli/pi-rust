//! Path helpers matching upstream `utils/paths.ts`, plus the `.gitignore`-style rules
//! resource discovery applies.

use std::path::{Component, Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};

const IGNORE_FILE_NAMES: [&str; 3] = [".gitignore", ".ignore", ".fdignore"];

/// `$HOME`, then `%USERPROFILE%`, then `.`.
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Remove `.` and `..` components without touching the filesystem, like Node `path.resolve`.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() && !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() { PathBuf::from(".") } else { out }
}

#[cfg(windows)]
fn normalize_windows_shell_path(path: &str) -> String {
    if !path.starts_with('/') || path.starts_with("//") || path.contains('\\') {
        return path.to_string();
    }
    let rest = path.strip_prefix("/mnt/").or_else(|| path.strip_prefix("/cygdrive/")).unwrap_or(&path[1..]);
    let mut chars = rest.chars();
    match (chars.next(), chars.next()) {
        (Some(drive), None) if drive.is_ascii_alphabetic() => format!("{}:\\", drive.to_ascii_uppercase()),
        (Some(drive), Some('/')) if drive.is_ascii_alphabetic() => {
            format!("{}:\\{}", drive.to_ascii_uppercase(), rest[2..].replace('/', "\\"))
        }
        _ => path.to_string(),
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = bytes.get(index + 1..index + 3).and_then(|hex| std::str::from_utf8(hex).ok());
        match (bytes[index], hex.and_then(|hex| u8::from_str_radix(hex, 16).ok())) {
            (b'%', Some(byte)) => {
                out.push(byte);
                index += 3;
            }
            (byte, _) => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn file_url_to_path(url: &str) -> PathBuf {
    let rest = url.trim_start_matches("file://");
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let decoded = percent_decode(rest);
    if cfg!(windows) {
        let trimmed = decoded.strip_prefix('/').filter(|path| path.as_bytes().get(1) == Some(&b':'));
        return PathBuf::from(trimmed.unwrap_or(&decoded).replace('/', "\\"));
    }
    PathBuf::from(decoded)
}

/// Expand `~`, convert `file://` URLs, and resolve against `base` (upstream `resolvePath`).
pub fn resolve_path(input: &str, base: &Path) -> PathBuf {
    let input = input.trim();
    #[cfg(windows)]
    let input = &normalize_windows_shell_path(input);
    let expanded = if input == "~" {
        home_dir()
    } else if let Some(rest) = input.strip_prefix("~/").or_else(|| input.strip_prefix("~\\").filter(|_| cfg!(windows)))
    {
        home_dir().join(rest)
    } else if input.starts_with("file://") {
        file_url_to_path(input)
    } else {
        PathBuf::from(input)
    };
    if expanded.is_absolute() { normalize(&expanded) } else { normalize(&base.join(expanded)) }
}

/// Expand a leading `~` and convert a `file://` URL, keeping relative text relative (upstream
/// `normalizePath` with its default options). Filter patterns go through it.
pub fn normalize_path_input(input: &str) -> String {
    let home = home_dir();
    #[cfg(windows)]
    let input = &normalize_windows_shell_path(input);
    if input == "~" {
        return home.to_string_lossy().into_owned();
    }
    let rest = input.strip_prefix("~/").or_else(|| input.strip_prefix("~\\").filter(|_| cfg!(windows)));
    if let Some(rest) = rest {
        let mut joined = normalize(&home.join(rest)).to_string_lossy().into_owned();
        if rest.ends_with(['/', '\\']) && !joined.ends_with(std::path::MAIN_SEPARATOR) {
            joined.push(std::path::MAIN_SEPARATOR);
        }
        return joined;
    }
    if input.starts_with("file://") {
        return file_url_to_path(input).to_string_lossy().into_owned();
    }
    input.to_string()
}

/// An absolute, normalized form of `path` (relative paths resolve against the process cwd).
pub fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return normalize(path);
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    normalize(&cwd.join(path))
}

/// The real path with symlinks resolved, or `path` itself when that fails.
pub fn canonicalize(path: &Path) -> PathBuf {
    match std::fs::canonicalize(path) {
        Ok(real) => strip_verbatim(real),
        Err(_) => path.to_path_buf(),
    }
}

#[cfg(windows)]
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy().into_owned();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => path,
    }
}

#[cfg(not(windows))]
fn strip_verbatim(path: PathBuf) -> PathBuf {
    path
}

/// False for package sources (`npm:`, `git:`, URLs) and `builtin:` extension paths.
pub fn is_local_path(value: &str) -> bool {
    let trimmed = value.trim();
    !["npm:", "marketplace:", "git:", "github:", "http:", "https:", "ssh:", "builtin:"]
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
}

/// True when `target` is `root` or inside it.
pub fn is_under_path(target: &Path, root: &Path) -> bool {
    normalize(target).starts_with(normalize(root))
}

/// `path` with forward slashes.
pub fn to_posix(path: &Path) -> String {
    let text = path.to_string_lossy();
    if std::path::MAIN_SEPARATOR == '/' { text.into_owned() } else { text.replace(std::path::MAIN_SEPARATOR, "/") }
}

/// Node `path.relative(base, target)` with forward slashes; empty when they are equal.
pub fn relative_posix(base: &Path, target: &Path) -> String {
    let base: Vec<Component> = base.components().collect();
    let target: Vec<Component> = target.components().collect();
    let common = base.iter().zip(&target).take_while(|(left, right)| left == right).count();
    let mut parts: Vec<String> = vec!["..".to_string(); base.len() - common];
    parts.extend(target[common..].iter().map(|part| part.as_os_str().to_string_lossy().into_owned()));
    parts.join("/")
}

/// Whether `path` (after following symlinks) is a file and whether it is a directory.
pub fn file_kind(path: &Path) -> Option<(bool, bool)> {
    std::fs::metadata(path).ok().map(|meta| (meta.is_file(), meta.is_dir()))
}

/// Directory entries sorted by name, so discovery order does not depend on the filesystem.
pub fn sorted_entries(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut entries: Vec<(String, PathBuf)> =
        entries.flatten().map(|entry| (entry.file_name().to_string_lossy().into_owned(), entry.path())).collect();
    entries.sort();
    entries
}

fn prefix_ignore_pattern(line: &str, prefix: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() || (trimmed.starts_with('#') && !trimmed.starts_with("\\#")) {
        return None;
    }
    let (negated, pattern) = match line.strip_prefix('!') {
        Some(rest) => (true, rest),
        None => (false, line.strip_prefix('\\').filter(|rest| rest.starts_with('!')).unwrap_or(line)),
    };
    let pattern = pattern.strip_prefix('/').unwrap_or(pattern);
    let prefixed = format!("{prefix}{pattern}");
    Some(if negated { format!("!{prefixed}") } else { prefixed })
}

/// Rules from `.gitignore`, `.ignore`, and `.fdignore` files met while walking down from `root`.
pub struct IgnoreRules {
    root: PathBuf,
    lines: Vec<String>,
    matcher: Option<Gitignore>,
}

impl IgnoreRules {
    pub fn new(root: &Path) -> Self {
        Self { root: root.to_path_buf(), lines: Vec::new(), matcher: None }
    }

    /// Add the ignore files in `dir`; their patterns are prefixed with its path relative to the root.
    pub fn add_dir(&mut self, dir: &Path) {
        let relative = relative_posix(&self.root, dir);
        let prefix = if relative.is_empty() { String::new() } else { format!("{relative}/") };
        let before = self.lines.len();
        for name in IGNORE_FILE_NAMES {
            let Ok(content) = std::fs::read_to_string(dir.join(name)) else { continue };
            self.lines.extend(content.lines().filter_map(|line| prefix_ignore_pattern(line, &prefix)));
        }
        if self.lines.len() != before {
            let mut builder = GitignoreBuilder::new(&self.root);
            for line in &self.lines {
                let _ = builder.add_line(None, line);
            }
            self.matcher = builder.build().ok();
        }
    }

    /// Whether `path` (relative to the root, with forward slashes) is ignored.
    pub fn ignores(&self, relative: &str, is_dir: bool) -> bool {
        self.matcher.as_ref().is_some_and(|matcher| matcher.matched(Path::new(relative), is_dir).is_ignore())
    }
}
