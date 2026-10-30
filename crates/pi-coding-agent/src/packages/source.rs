//! Package sources: `npm:`, `marketplace:`, git URLs, URL archives, and local paths.
//! Extends `parseSource` from upstream `core/package-manager.ts`.

use nodejs_semver::{Range, Version};
use reqwest::Url;

use super::git_url::{GitSource, parse_git_url};
use crate::resources::paths::is_local_path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NpmSource {
    /// The spec after `npm:`, such as `@scope/pkg@^1.2.0`.
    pub spec: String,
    pub name: String,
    pub version: Option<String>,
    /// True when `version` is a valid semver range (exact versions are ranges too).
    pub has_range: bool,
    /// True for an exact version; updates leave pinned packages alone.
    pub pinned: bool,
}

impl NpmSource {
    /// Whether `installed` satisfies the configured range (always true without one).
    pub fn accepts(&self, installed: &str) -> bool {
        match (&self.version, self.has_range) {
            (Some(range), true) => match (Range::parse(range), Version::parse(installed)) {
                (Ok(range), Ok(version)) => range.satisfies(&version),
                _ => false,
            },
            _ => true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveFormat {
    TarGz,
    Zip,
}

/// An `http` or `https` URL of a `.tar.gz`, `.tgz`, or `.zip` package archive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveSource {
    pub url: String,
    pub host: String,
    /// The URL path without the archive extension, used for the install directory.
    pub path: String,
    pub format: ArchiveFormat,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalSource {
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParsedSource {
    Npm(NpmSource),
    Git(GitSource),
    Archive(ArchiveSource),
    Marketplace { name: String, tag: Option<String> },
    Local(LocalSource),
}

impl ParsedSource {
    pub fn is_pinned(&self) -> bool {
        match self {
            Self::Npm(npm) => npm.pinned,
            Self::Git(git) => git.pinned,
            Self::Marketplace { tag, .. } => tag.is_some(),
            Self::Archive(_) | Self::Local(_) => false,
        }
    }
}

/// Split `name@version`, keeping a leading `@scope/` in the name.
pub fn parse_npm_spec(spec: &str) -> (String, Option<String>) {
    let start = usize::from(spec.starts_with('@'));
    let Some(offset) = spec[start..].find('@') else { return (spec.to_string(), None) };
    let at = start + offset;
    let version = &spec[at + 1..];
    if at == start || version.is_empty() {
        return (spec.to_string(), None);
    }
    (spec[..at].to_string(), Some(version.to_string()))
}

pub fn is_exact_version(version: &str) -> bool {
    Version::parse(version).is_ok()
}

pub fn is_valid_range(version: &str) -> bool {
    Range::parse(version).is_ok()
}

/// `a > b` in semver order; an error when either is not a valid version.
pub fn version_gt(a: &str, b: &str) -> anyhow::Result<bool> {
    let parse = |value: &str| Version::parse(value).map_err(|_| anyhow::anyhow!("Invalid Version: {value}"));
    Ok(parse(a)? > parse(b)?)
}

/// The highest of `versions` within `range`, or the highest overall without a range.
pub fn max_version(versions: &[String], range: Option<&str>) -> Option<String> {
    let mut parsed: Vec<Version> = versions.iter().filter_map(|value| Version::parse(value).ok()).collect();
    if let Some(range) = range {
        let range = Range::parse(range).ok()?;
        return range.max_satisfying(&parsed).map(ToString::to_string);
    }
    parsed.sort();
    parsed.pop().map(|version| version.to_string())
}

pub fn parse_npm_source(spec: &str) -> NpmSource {
    let spec = spec.trim().to_string();
    let (name, version) = parse_npm_spec(&spec);
    let has_range = version.as_deref().is_some_and(is_valid_range);
    let pinned = version.as_deref().is_some_and(is_exact_version);
    NpmSource { spec, name, version, has_range, pinned }
}

fn archive_format(path: &str) -> Option<(ArchiveFormat, &str)> {
    let lower = path.to_ascii_lowercase();
    let formats = [(".tar.gz", ArchiveFormat::TarGz), (".tgz", ArchiveFormat::TarGz), (".zip", ArchiveFormat::Zip)];
    formats
        .into_iter()
        .find(|(extension, _)| lower.ends_with(extension))
        .map(|(extension, format)| (format, &path[..path.len() - extension.len()]))
}

fn safe_archive_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains(['\\', '\0'])
        && path.split('/').all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

/// Recognize a URL archive source; other URLs are left to the git parser.
pub fn parse_archive_url(source: &str) -> Option<ArchiveSource> {
    let trimmed = source.trim();
    let url = Url::parse(trimmed).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_string();
    let path = url.path().trim_start_matches('/');
    let (format, stem) = archive_format(path)?;
    let decoded = super::git_url::decode_component(stem)?;
    let safe = safe_archive_path(stem) && safe_archive_path(&decoded);
    safe.then(|| ArchiveSource { url: trimmed.to_string(), host, path: stem.to_string(), format })
}

/// Classify a package source the way upstream `parseSource` does, with URL archives
/// recognized before git URLs.
pub fn parse_source(source: &str) -> ParsedSource {
    if let Some(spec) = source.strip_prefix("marketplace:") {
        let (name, tag) = spec.split_once('@').map_or((spec, None), |(name, tag)| (name, Some(tag.to_string())));
        return ParsedSource::Marketplace { name: name.to_string(), tag };
    }
    if let Some(spec) = source.strip_prefix("npm:") {
        return ParsedSource::Npm(parse_npm_source(spec));
    }
    if is_local_path(source) {
        return ParsedSource::Local(LocalSource { path: source.to_string() });
    }
    if let Some(archive) = parse_archive_url(source) {
        return ParsedSource::Archive(archive);
    }
    match parse_git_url(source) {
        Some(git) => ParsedSource::Git(git),
        None => ParsedSource::Local(LocalSource { path: source.to_string() }),
    }
}
