//! Where a resource came from, ported from upstream `core/source-info.ts` and the
//! `PathMetadata` and `ResolvedPaths` types of `core/package-manager.ts`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Prefix of built-in extension paths, such as `builtin:mcp`.
pub const BUILTIN_PATH_PREFIX: &str = "builtin:";
/// Paths of `pi.native` programs: `native:<package name>/<name>`.
pub const NATIVE_PATH_PREFIX: &str = "native:";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceScope {
    User,
    Project,
    #[default]
    Temporary,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceOrigin {
    Package,
    #[default]
    TopLevel,
}

/// The source of a loaded resource: `local`, `auto`, `cli`, `builtin`, or a package source
/// such as `npm:foo`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceInfo {
    pub path: String,
    pub source: String,
    pub scope: SourceScope,
    pub origin: SourceOrigin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_dir: Option<String>,
}

/// Metadata of a resolved resource path, before it becomes a [`SourceInfo`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathMetadata {
    pub source: String,
    pub scope: SourceScope,
    pub origin: SourceOrigin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_root: Option<PathBuf>,
}

impl PathMetadata {
    pub fn new(source: &str, scope: SourceScope, origin: SourceOrigin) -> Self {
        Self { source: source.into(), scope, origin, base_dir: None, package_root: None }
    }

    pub fn with_base_dir(mut self, base_dir: impl Into<PathBuf>) -> Self {
        self.base_dir = Some(base_dir.into());
        self
    }

    /// Load order: project settings, project auto, user settings, user auto, packages, built-ins.
    pub fn precedence_rank(&self) -> u8 {
        if self.source == "builtin" {
            return 5;
        }
        if self.origin == SourceOrigin::Package {
            return 4;
        }
        let scope_base = if self.scope == SourceScope::Project { 0 } else { 2 };
        scope_base + u8::from(self.source != "local")
    }
}

/// A discovered resource file and whether settings filters leave it enabled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedResource {
    pub path: PathBuf,
    pub enabled: bool,
    pub metadata: PathMetadata,
}

/// Resolved resource files by type. The package manager produces these for packages.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResolvedPaths {
    pub extensions: Vec<ResolvedResource>,
    pub skills: Vec<ResolvedResource>,
    pub prompts: Vec<ResolvedResource>,
    pub themes: Vec<ResolvedResource>,
}

/// `builtin` for `builtin:<name>`, the prefix of `<inline:name>`, or `None` for file paths.
pub fn synthetic_path_source(path: &str) -> Option<String> {
    if path.starts_with(BUILTIN_PATH_PREFIX) {
        return Some("builtin".into());
    }
    let inner = path.strip_prefix('<')?.strip_suffix('>')?;
    let source = inner.split(':').next().unwrap_or_default();
    Some(if source.is_empty() { "temporary".into() } else { source.into() })
}

pub fn is_synthetic_path(path: &str) -> bool {
    path.starts_with(BUILTIN_PATH_PREFIX) || path.starts_with(NATIVE_PATH_PREFIX) || path.starts_with('<')
}

pub fn create_source_info(path: &str, metadata: &PathMetadata) -> SourceInfo {
    SourceInfo {
        path: path.into(),
        source: metadata.source.clone(),
        scope: metadata.scope,
        origin: metadata.origin,
        base_dir: metadata.base_dir.as_ref().map(|dir| dir.display().to_string()),
    }
}

/// A source info for a resource with no package metadata: top-level, temporary unless given.
pub fn create_synthetic_source_info(
    path: &str,
    source: &str,
    scope: Option<SourceScope>,
    base_dir: Option<&std::path::Path>,
) -> SourceInfo {
    SourceInfo {
        path: path.into(),
        source: source.into(),
        scope: scope.unwrap_or_default(),
        origin: SourceOrigin::TopLevel,
        base_dir: base_dir.map(|dir| dir.display().to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_paths_and_precedence() {
        assert_eq!(synthetic_path_source("builtin:mcp").as_deref(), Some("builtin"));
        assert_eq!(synthetic_path_source("<inline:x>").as_deref(), Some("inline"));
        assert_eq!(synthetic_path_source("</a>").as_deref(), Some("/a"));
        assert_eq!(synthetic_path_source("/a/b.ts"), None);
        assert!(is_synthetic_path("<inline:1>"));
        let rank = |source: &str, scope, origin| PathMetadata::new(source, scope, origin).precedence_rank();
        assert_eq!(rank("local", SourceScope::Project, SourceOrigin::TopLevel), 0);
        assert_eq!(rank("auto", SourceScope::Project, SourceOrigin::TopLevel), 1);
        assert_eq!(rank("local", SourceScope::User, SourceOrigin::TopLevel), 2);
        assert_eq!(rank("auto", SourceScope::User, SourceOrigin::TopLevel), 3);
        assert_eq!(rank("npm:x", SourceScope::User, SourceOrigin::Package), 4);
        assert_eq!(rank("builtin", SourceScope::User, SourceOrigin::TopLevel), 5);
        let info = create_source_info("/p", &PathMetadata::new("auto", SourceScope::User, SourceOrigin::TopLevel));
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json, serde_json::json!({"path": "/p", "source": "auto", "scope": "user", "origin": "top-level"}));
    }
}
