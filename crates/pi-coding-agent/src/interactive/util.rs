//! Small formatting helpers shared by interactive mode (upstream `utils/paths.ts` and friends).

use std::path::{Component, Path, PathBuf};

use crate::config::home_dir;

/// A number as JavaScript prints it: integers without a fraction, others in shortest form.
pub fn js_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e21 { format!("{}", value as i64) } else { format!("{value}") }
}

/// Lexically resolve `path` against `base` (upstream `path.resolve`).
pub fn resolve_lexically(path: &Path, base: &Path) -> PathBuf {
    let joined = if path.is_absolute() { path.to_path_buf() } else { base.join(path) };
    let mut out = PathBuf::new();
    for part in joined.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `file_path` relative to `cwd` when inside it (`.` for `cwd` itself).
pub fn cwd_relative_path(file_path: &Path, cwd: &Path) -> Option<String> {
    let cwd = resolve_lexically(cwd, Path::new("/"));
    let resolved = resolve_lexically(file_path, &cwd);
    let relative = resolved.strip_prefix(&cwd).ok()?;
    let text = relative.display().to_string();
    Some(if text.is_empty() { ".".to_string() } else { text })
}

/// Relative to `cwd` when inside it, else absolute; always with `/` separators.
pub fn format_path_relative_to_cwd_or_absolute(file_path: &Path, cwd: &Path) -> String {
    let absolute = resolve_lexically(file_path, cwd);
    let text = cwd_relative_path(&absolute, cwd).unwrap_or_else(|| absolute.display().to_string());
    text.replace(std::path::MAIN_SEPARATOR, "/")
}

/// Replace the home directory prefix with `~`.
pub fn format_display_path(path: &str) -> String {
    let home = home_dir().display().to_string();
    match path.strip_prefix(&home) {
        Some(rest) if !home.is_empty() => format!("~{rest}"),
        _ => path.to_string(),
    }
}

/// Compact token counts for the footer: `999`, `1.2k`, `45k`, `1.5M`, `12M`.
pub fn format_tokens(count: u64) -> String {
    let count_f = count as f64;
    match count {
        0..1_000 => count.to_string(),
        1_000..10_000 => format!("{:.1}k", count_f / 1000.0),
        10_000..1_000_000 => format!("{}k", (count_f / 1000.0).round() as u64),
        1_000_000..10_000_000 => format!("{:.1}M", count_f / 1_000_000.0),
        _ => format!("{}M", (count_f / 1_000_000.0).round() as u64),
    }
}

/// Quote a shell argument unless it only has safe characters.
pub fn quote_if_needed(value: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "_-./~:@".contains(c);
    if !value.is_empty() && value.chars().all(safe) {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_numbers_like_javascript() {
        assert_eq!(js_number(30.0), "30");
        assert_eq!(js_number(1.5), "1.5");
    }

    #[test]
    fn formats_token_counts() {
        let values = [999, 1_000, 1_234, 9_999, 10_000, 45_499, 999_499, 1_500_000, 12_400_000];
        let formatted: Vec<String> = values.iter().map(|value| format_tokens(*value)).collect();
        assert_eq!(formatted, ["999", "1.0k", "1.2k", "10.0k", "10k", "45k", "999k", "1.5M", "12M"]);
    }

    #[test]
    fn relative_paths_stay_inside_cwd() {
        let cwd = Path::new("/work/project");
        assert_eq!(cwd_relative_path(Path::new("/work/project/src/a.rs"), cwd).as_deref(), Some("src/a.rs"));
        assert_eq!(cwd_relative_path(Path::new("/work/project"), cwd).as_deref(), Some("."));
        assert_eq!(cwd_relative_path(Path::new("/work/other"), cwd), None);
        assert_eq!(format_path_relative_to_cwd_or_absolute(Path::new("../x"), cwd), "/work/x");
    }

    #[test]
    fn quotes_shell_arguments() {
        assert_eq!(quote_if_needed("/tmp/a.png"), "/tmp/a.png");
        assert_eq!(quote_if_needed("a b"), "'a b'");
        assert_eq!(quote_if_needed("it's"), "'it'\\''s'");
    }
}
