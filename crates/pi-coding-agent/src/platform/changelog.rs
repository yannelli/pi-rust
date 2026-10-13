//! Changelog parsing, the "What's New" startup view, `/changelog`, and the condensed notice for
//! `collapseChangelog` (upstream `utils/changelog.ts`). The source is this repository's
//! CHANGELOG.md, embedded at build time.

use std::sync::LazyLock;

use regex::Regex;

pub const EMBEDDED_CHANGELOG: &str = include_str!("../../../../CHANGELOG.md");

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangelogEntry {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// The section including its `## [x.y.z]` header, trimmed.
    pub content: String,
}

impl ChangelogEntry {
    pub fn version(&self) -> String {
        format!("{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Where relative changelog links point: a GitHub repository and the changelog's directory in it.
#[derive(Clone, Copy, Debug)]
pub struct ChangelogRepo {
    pub github_repo: &'static str,
    pub link_base_path: &'static str,
    /// Old repository URLs rewritten to `github_repo`.
    pub legacy_repo_urls: &'static [&'static str],
}

pub const PI_RUST_CHANGELOG_REPO: ChangelogRepo =
    ChangelogRepo { github_repo: "yannelli/pi-rust", link_base_path: "", legacy_repo_urls: &[] };

static VERSION_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"##\s+\[?(\d+)\.(\d+)\.(\d+)\]?").expect("valid pattern"));
static URL_SCHEME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^[a-z][a-z0-9+.-]*:").expect("valid pattern"));
static INLINE_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(!?\[[^\]\n]+\]\()([^\s)]+)((?:\s+[^)]*)?\))").expect("valid pattern"));

/// Entries in file order, one per `## [x.y.z]` section; other `##` sections are skipped.
pub fn parse_changelog(content: &str) -> Vec<ChangelogEntry> {
    let mut entries = Vec::new();
    let mut current: Option<(u64, u64, u64)> = None;
    let mut lines: Vec<&str> = Vec::new();
    let mut flush = |current: Option<(u64, u64, u64)>, lines: &[&str]| {
        if let Some((major, minor, patch)) = current
            && !lines.is_empty()
        {
            entries.push(ChangelogEntry { major, minor, patch, content: lines.join("\n").trim().to_string() });
        }
    };
    for line in content.split('\n') {
        if line.starts_with("## ") {
            flush(current, &lines);
            let parsed = VERSION_HEADER.captures(line).and_then(|captures| {
                let number = |index: usize| captures[index].parse::<u64>().ok();
                Some((number(1)?, number(2)?, number(3)?))
            });
            current = parsed;
            lines = if parsed.is_some() { vec![line] } else { Vec::new() };
        } else if current.is_some() {
            lines.push(line);
        }
    }
    flush(current, &lines);
    entries
}

/// Entries of the embedded CHANGELOG.md.
pub fn embedded_changelog_entries() -> Vec<ChangelogEntry> {
    parse_changelog(EMBEDDED_CHANGELOG)
}

fn version_key(entry: &ChangelogEntry) -> (u64, u64, u64) {
    (entry.major, entry.minor, entry.patch)
}

/// Entries newer than `last_version`; unparsable parts count as 0.
pub fn get_new_entries(entries: &[ChangelogEntry], last_version: &str) -> Vec<ChangelogEntry> {
    let mut parts = last_version.split('.').map(|part| part.parse::<u64>().unwrap_or(0));
    let last = (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    entries.iter().filter(|entry| version_key(entry) > last).cloned().collect()
}

/// What startup does with the changelog (upstream `getChangelogForDisplay`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StartupChangelog {
    /// Markdown for the "What's New" view.
    pub markdown: Option<String>,
    /// Store the running version as `lastChangelogVersion`.
    pub record_version: bool,
    /// Send the install ping when telemetry allows it.
    pub report_install: bool,
}

/// Nothing for resumed sessions; on a fresh install only record the version; after an update
/// show the entries newer than `last_version`.
pub fn startup_changelog(
    entries: &[ChangelogEntry],
    last_version: Option<&str>,
    has_messages: bool,
) -> StartupChangelog {
    if has_messages {
        return StartupChangelog::default();
    }
    let Some(last_version) = last_version.filter(|version| !version.is_empty()) else {
        return StartupChangelog { markdown: None, record_version: true, report_install: true };
    };
    let new_entries = get_new_entries(entries, last_version);
    if new_entries.is_empty() {
        return StartupChangelog::default();
    }
    let markdown = new_entries.iter().map(|entry| normalize_changelog_links(&entry.content, &entry.version()));
    StartupChangelog {
        markdown: Some(markdown.collect::<Vec<_>>().join("\n\n")),
        record_version: true,
        report_install: true,
    }
}

/// The version named by the first header of `markdown`, else `current_version`.
pub fn condensed_changelog_version(markdown: &str, current_version: &str) -> String {
    static HEADER: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"##\s+\[?(\d+\.\d+\.\d+)\]?").expect("valid pattern"));
    HEADER.captures(markdown).map(|captures| captures[1].to_string()).unwrap_or_else(|| current_version.into())
}

/// The one-line notice shown instead of the changelog when `collapseChangelog` is on.
pub fn condensed_changelog_notice(markdown: &str, current_version: &str) -> String {
    let version = condensed_changelog_version(markdown, current_version);
    format!("Updated to v{version}. Use /changelog to view full changelog.")
}

/// `/changelog`: every entry, oldest first.
pub fn changelog_command_markdown(entries: &[ChangelogEntry]) -> String {
    if entries.is_empty() {
        return "No changelog entries found.".into();
    }
    let sections = entries.iter().rev().map(|entry| normalize_changelog_links(&entry.content, &entry.version()));
    sections.collect::<Vec<_>>().join("\n\n")
}

fn normalize_posix(path: &str) -> String {
    let absolute = path.starts_with('/');
    let trailing = path.ends_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." if parts.last().is_some_and(|last| *last != "..") => {
                parts.pop();
            }
            ".." if absolute => {}
            other => parts.push(other),
        }
    }
    let mut joined = parts.join("/");
    if absolute {
        joined.insert(0, '/');
    }
    if joined.is_empty() {
        joined.push('.');
    }
    if trailing && !joined.ends_with('/') {
        joined.push('/');
    }
    joined
}

fn resolve_repository_path(target: &str, base: &str) -> Option<String> {
    let target = target.replace('\\', "/");
    let joined = match target.strip_prefix('/') {
        Some(_) => normalize_posix(target.trim_start_matches('/')),
        None if base.is_empty() => normalize_posix(&target),
        None => normalize_posix(&format!("{base}/{target}")),
    };
    (joined != "." && joined != ".." && !joined.starts_with("../")).then_some(joined)
}

fn encode_uri(text: &str) -> String {
    const KEEP: &[u8] = b";,/?:@&=+$-_.!~*'()#";
    let mut encoded = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || KEEP.contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn normalize_link_target(target: &str, tag: &str, repo: &ChangelogRepo) -> String {
    let repo_url = format!("https://github.com/{}", repo.github_repo);
    let mut canonical = target.to_string();
    for legacy in repo.legacy_repo_urls {
        if let Some(rest) = target.strip_prefix(legacy)
            && (rest.is_empty() || rest.starts_with('/'))
        {
            canonical = format!("{repo_url}{rest}");
        }
    }
    for route in ["blob", "tree"] {
        for branch in ["main", "master"] {
            let prefix = format!("{repo_url}/{route}/{branch}/");
            if let Some(rest) = canonical.strip_prefix(&prefix) {
                canonical = format!("{repo_url}/{route}/{tag}/{rest}");
            }
        }
    }
    if canonical.starts_with('#') || canonical.starts_with("//") || URL_SCHEME.is_match(&canonical) {
        return canonical;
    }
    let (before_hash, fragment) = canonical.split_at(canonical.find('#').unwrap_or(canonical.len()));
    let (path_part, query) = before_hash.split_at(before_hash.find('?').unwrap_or(before_hash.len()));
    if path_part.is_empty() {
        return canonical;
    }
    let Some(repository_path) = resolve_repository_path(path_part, repo.link_base_path) else { return canonical };
    let basename = repository_path.trim_end_matches('/').rsplit('/').next().unwrap_or_default();
    let route = if path_part.ends_with('/') || !basename.contains('.') { "tree" } else { "blob" };
    format!("https://github.com/{}/{route}/{tag}/{}{query}{fragment}", repo.github_repo, encode_uri(&repository_path))
}

/// Pin relative and branch links in `markdown` to the tag of `version` in `repo`.
pub fn normalize_changelog_links_in(markdown: &str, version: &str, repo: &ChangelogRepo) -> String {
    let tag = if version.starts_with('v') { version.to_string() } else { format!("v{version}") };
    INLINE_LINK
        .replace_all(markdown, |captures: &regex::Captures<'_>| {
            format!("{}{}{}", &captures[1], normalize_link_target(&captures[2], &tag, repo), &captures[3])
        })
        .into_owned()
}

/// [`normalize_changelog_links_in`] for this repository.
pub fn normalize_changelog_links(markdown: &str, version: &str) -> String {
    normalize_changelog_links_in(markdown, version, &PI_RUST_CHANGELOG_REPO)
}

#[cfg(test)]
#[path = "changelog_tests.rs"]
mod tests;
