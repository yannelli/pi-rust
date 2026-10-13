//! Prefix extraction, quoting, and POSIX-style path helpers shared by the completion sources.

use std::cmp::Ordering;

use crate::utils::AUTOCOMPLETE_SEPARATOR_REGEX;

/// Characters that end a path token, besides whitespace and CJK punctuation.
const PATH_DELIMITERS: [char; 5] = [' ', '\t', '"', '\'', '='];

/// Opening wrappers that may precede a path in prose, with their closers.
const PATH_WRAPPERS: [(char, char); 5] = [('(', ')'), ('[', ']'), ('{', '}'), ('<', '>'), ('`', '`')];

fn wrapper_closer(c: char) -> Option<char> {
    PATH_WRAPPERS.iter().find(|(open, _)| *open == c).map(|(_, close)| *close)
}

/// Whitespace or CJK punctuation that separates a completion token from preceding prose.
pub(crate) fn is_autocomplete_separator(c: char) -> bool {
    AUTOCOMPLETE_SEPARATOR_REGEX.is_match(c.encode_utf8(&mut [0; 4]))
}

/// True for empty text or text that ends with a separator (upstream `tokenStartRegex`).
pub(crate) fn ends_at_token_start(text: &str) -> bool {
    text.chars().next_back().is_none_or(is_autocomplete_separator)
}

/// Replaces backslashes with slashes.
pub(crate) fn to_display_path(value: &str) -> String {
    value.replace('\\', "/")
}

/// Byte offset just after the last delimiter, or `None` without one.
pub(crate) fn find_last_delimiter_end(text: &str) -> Option<usize> {
    text.char_indices()
        .rfind(|(_, c)| PATH_DELIMITERS.contains(c) || is_autocomplete_separator(*c))
        .map(|(index, c)| index + c.len_utf8())
}

/// The token after the last delimiter.
fn last_token(text: &str) -> &str {
    find_last_delimiter_end(text).map_or(text, |end| &text[end..])
}

/// Strips opening wrappers before a path (`(~/Dev` gives `~/Dev`) unless the token also contains
/// the closer (`app/[slug]/pa` and `(group)/pa` keep theirs).
pub(crate) fn strip_leading_wrappers(token: &str) -> &str {
    let mut result = token;
    while let Some(first) = result.chars().next() {
        let Some(closer) = wrapper_closer(first) else { break };
        if result[first.len_utf8()..].contains(closer) {
            break;
        }
        result = &result[first.len_utf8()..];
    }
    result
}

fn find_unclosed_quote_start(text: &str) -> Option<usize> {
    let mut quote_start = None;
    let mut in_quotes = false;
    for (index, byte) in text.bytes().enumerate() {
        if byte == b'"' {
            in_quotes = !in_quotes;
            if in_quotes {
                quote_start = Some(index);
            }
        }
    }
    if in_quotes { quote_start } else { None }
}

fn is_token_start(text: &str, index: usize) -> bool {
    let bytes = text.as_bytes();
    let mut start = index;
    while start > 0 && wrapper_closer(char::from(bytes[start - 1])).is_some() && bytes[start - 1].is_ascii() {
        start -= 1;
    }
    let after_delimiter =
        start > 0 && bytes[start - 1].is_ascii() && PATH_DELIMITERS.contains(&char::from(bytes[start - 1]));
    after_delimiter || ends_at_token_start(&text[..start])
}

fn extract_quoted_prefix(text: &str) -> Option<&str> {
    let quote_start = find_unclosed_quote_start(text)?;
    if quote_start > 0 && text.as_bytes()[quote_start - 1] == b'@' {
        return is_token_start(text, quote_start - 1).then(|| &text[quote_start - 1..]);
    }
    is_token_start(text, quote_start).then(|| &text[quote_start..])
}

/// A path prefix split into its raw path and its `@` and quote markers.
pub(crate) struct PathPrefix<'a> {
    pub raw: &'a str,
    pub is_at: bool,
    pub is_quoted: bool,
}

pub(crate) fn parse_path_prefix(prefix: &str) -> PathPrefix<'_> {
    if let Some(raw) = prefix.strip_prefix("@\"") {
        PathPrefix { raw, is_at: true, is_quoted: true }
    } else if let Some(raw) = prefix.strip_prefix('"') {
        PathPrefix { raw, is_at: false, is_quoted: true }
    } else if let Some(raw) = prefix.strip_prefix('@') {
        PathPrefix { raw, is_at: true, is_quoted: false }
    } else {
        PathPrefix { raw: prefix, is_at: false, is_quoted: false }
    }
}

/// Adds the `@` marker and quotes a path that needs them.
pub(crate) fn build_completion_value(path: &str, is_at: bool, is_quoted: bool) -> String {
    let needs_quotes = is_quoted || AUTOCOMPLETE_SEPARATOR_REGEX.is_match(path);
    let prefix = if is_at { "@" } else { "" };
    if needs_quotes { format!("{prefix}\"{path}\"") } else { format!("{prefix}{path}") }
}

/// The `@` attachment prefix before the cursor, if any.
pub(crate) fn extract_at_prefix(text: &str) -> Option<String> {
    if let Some(quoted) = extract_quoted_prefix(text).filter(|q| q.starts_with("@\"")) {
        return Some(quoted.to_string());
    }
    let token = strip_leading_wrappers(last_token(text));
    token.starts_with('@').then(|| token.to_string())
}

/// A path-like prefix before the cursor. `force` (Tab) always returns the last token.
pub(crate) fn extract_path_prefix(text: &str, force: bool) -> Option<String> {
    if let Some(quoted) = extract_quoted_prefix(text) {
        return Some(quoted.to_string());
    }
    let prefix = strip_leading_wrappers(last_token(text));
    if force || prefix.contains('/') || prefix.starts_with('.') || prefix.starts_with("~/") {
        return Some(prefix.to_string());
    }
    // An empty prefix after a separator still completes; empty text needs an explicit Tab.
    (prefix.is_empty() && !text.is_empty() && ends_at_token_start(text)).then(String::new)
}

/// Node `path.posix.normalize`.
pub(crate) fn normalize(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let absolute = path.starts_with('/');
    let trailing = path.ends_with('/');
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if segments.last().is_some_and(|last| *last != "..") {
                    segments.pop();
                } else if !absolute {
                    segments.push("..");
                }
            }
            other => segments.push(other),
        }
    }
    let mut result = segments.join("/");
    if result.is_empty() {
        return match (absolute, trailing) {
            (true, _) => "/".to_string(),
            (false, true) => "./".to_string(),
            (false, false) => ".".to_string(),
        };
    }
    if trailing {
        result.push('/');
    }
    if absolute { format!("/{result}") } else { result }
}

/// Node `path.posix.join`.
pub(crate) fn join(parts: &[&str]) -> String {
    let joined: Vec<&str> = parts.iter().copied().filter(|part| !part.is_empty()).collect();
    if joined.is_empty() { ".".to_string() } else { normalize(&joined.join("/")) }
}

/// Node `path.posix.dirname`.
pub(crate) fn dirname(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let bytes = path.as_bytes();
    let has_root = bytes[0] == b'/';
    let mut end = None;
    let mut matched_slash = true;
    for i in (1..bytes.len()).rev() {
        if bytes[i] == b'/' {
            if !matched_slash {
                end = Some(i);
                break;
            }
        } else {
            matched_slash = false;
        }
    }
    match end {
        None => (if has_root { "/" } else { "." }).to_string(),
        Some(1) if has_root => "//".to_string(),
        Some(end) => path[..end].to_string(),
    }
}

/// Node `path.posix.basename`.
pub(crate) fn basename(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut start = 0;
    let mut end = None;
    for i in (0..bytes.len()).rev() {
        if bytes[i] == b'/' {
            if end.is_some() {
                start = i + 1;
                break;
            }
        } else if end.is_none() {
            end = Some(i + 1);
        }
    }
    end.map_or_else(String::new, |end| path[start..end].to_string())
}

/// Collation class: punctuation and symbols, then digits, then letters.
fn primary_key(c: char) -> (u8, char) {
    let class = if c.is_alphabetic() {
        2
    } else if c.is_numeric() {
        1
    } else {
        0
    };
    (class, c.to_lowercase().next().unwrap_or(c))
}

/// Approximates `String.prototype.localeCompare` (ICU root collation): case-insensitive first,
/// then lowercase before uppercase.
pub(crate) fn locale_compare(a: &str, b: &str) -> Ordering {
    a.chars()
        .map(primary_key)
        .cmp(b.chars().map(primary_key))
        .then_with(|| a.chars().map(char::is_uppercase).cmp(b.chars().map(char::is_uppercase)))
        .then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posix_helpers_match_node() {
        assert_eq!(normalize("./src/../lib/"), "lib/");
        assert_eq!(normalize("../a/.."), "..");
        assert_eq!(normalize("/a/../../b"), "/b");
        assert_eq!(join(&["/tmp/base", "../outside/"]), "/tmp/outside/");
        assert_eq!(join(&["./src", "main.ts"]), "src/main.ts");
        assert_eq!(dirname("a/b/c"), "a/b");
        assert_eq!(dirname("a"), ".");
        assert_eq!(dirname("/a"), "/");
        assert_eq!(dirname("a/b/"), "a");
        assert_eq!(basename("a/b/"), "b");
        assert_eq!(basename("~/Dev"), "Dev");
    }

    #[test]
    fn extracts_prefixes() {
        assert_eq!(extract_at_prefix("see (@src").as_deref(), Some("@src"));
        assert_eq!(extract_at_prefix("foo(@src"), None);
        assert_eq!(extract_at_prefix("user@example.com"), None);
        assert_eq!(extract_at_prefix("\u{67e5}\u{770b}\u{ff0c}@REA").as_deref(), Some("@REA"));
        assert_eq!(extract_path_prefix("hey /", true).as_deref(), Some("/"));
        assert_eq!(extract_path_prefix("hey ", false).as_deref(), Some(""));
        assert_eq!(extract_path_prefix("", false), None);
        assert_eq!(extract_path_prefix("app/[slug]/pa", false).as_deref(), Some("app/[slug]/pa"));
        assert_eq!(strip_leading_wrappers("(~/Dev"), "~/Dev");
        assert_eq!(strip_leading_wrappers("(group)/pa"), "(group)/pa");
    }

    #[test]
    fn compares_like_locale_compare() {
        let mut names = vec!["b", "B", "a", "_x", "10", "A"];
        names.sort_by(|a, b| locale_compare(a, b));
        assert_eq!(names, ["_x", "10", "a", "A", "b", "B"]);
    }
}
