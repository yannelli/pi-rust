//! `-e` extension sources and the source info of loaded resources, ported from the
//! resource loader and the local-source parts of upstream `core/package-manager.ts`.

use std::path::{Path, PathBuf};

use super::brand::CONFIG_DIR_NAME;
use super::discovery::{ResourceAccumulator, ResourceType, collect_resource_files};
use super::paths::{canonicalize, file_kind, is_local_path, is_under_path, normalize, resolve_path};
use super::source_info::{
    BUILTIN_PATH_PREFIX, PathMetadata, ResolvedPaths, SourceInfo, SourceOrigin, SourceScope, create_source_info,
    is_synthetic_path, synthetic_path_source,
};

/// Resources named by `-e`: `builtin:<name>` paths and local files or directories. Package
/// sources (`npm:`, `git:`, a directory with a `pi` manifest) come resolved in `packages`.
pub fn resolve_extension_sources(sources: &[String], cwd: &Path, packages: &ResolvedPaths) -> ResolvedPaths {
    let mut acc = ResourceAccumulator::default();
    for source in sources {
        if source.starts_with(BUILTIN_PATH_PREFIX) {
            let metadata = PathMetadata::new("builtin", SourceScope::Temporary, SourceOrigin::TopLevel);
            acc.add(ResourceType::Extensions, PathBuf::from(source), metadata, true);
            continue;
        }
        if !is_local_path(source) {
            continue;
        }
        let path = resolve_path(source, cwd);
        let mut metadata = PathMetadata::new(source, SourceScope::Temporary, SourceOrigin::Package);
        match file_kind(&path) {
            Some((true, _)) => {
                metadata.base_dir = path.parent().map(Path::to_path_buf);
                acc.add(ResourceType::Extensions, path, metadata, true);
            }
            Some((_, true)) if !has_pi_manifest(&path) => {
                metadata.base_dir = Some(path.clone());
                metadata.package_root = Some(path.clone());
                let mut found = false;
                for kind in ResourceType::ALL {
                    let dir = path.join(kind.dir_name());
                    if dir.exists() {
                        found = true;
                        for file in collect_resource_files(&dir, kind) {
                            acc.add(kind, file, metadata.clone(), true);
                        }
                    }
                }
                if !found {
                    acc.add(ResourceType::Extensions, path, metadata, true);
                }
            }
            _ => {}
        }
    }
    let lists = [&packages.extensions, &packages.skills, &packages.prompts, &packages.themes];
    for (kind, list) in ResourceType::ALL.into_iter().zip(lists) {
        acc.add_all(kind, list);
    }
    acc.into_resolved()
}

fn has_pi_manifest(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("package.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(text.trim_start_matches('\u{feff}')).ok())
        .is_some_and(|manifest| manifest.get("pi").is_some_and(serde_json::Value::is_object))
}

/// Path metadata recorded during a reload, looked up to build each resource's [`SourceInfo`].
#[derive(Clone, Debug, Default)]
pub struct SourceLookup {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    metadata: Vec<(PathBuf, PathMetadata)>,
}

impl SourceLookup {
    pub fn new(cwd: &Path, agent_dir: &Path) -> Self {
        Self { cwd: cwd.to_path_buf(), agent_dir: agent_dir.to_path_buf(), metadata: Vec::new() }
    }

    pub fn has(&self, path: &Path) -> bool {
        self.metadata.iter().any(|(known, _)| known == path)
    }

    /// Record metadata for `path` unless it already has some.
    pub fn record(&mut self, path: &Path, metadata: &PathMetadata) {
        if !self.has(path) {
            self.metadata.push((path.to_path_buf(), metadata.clone()));
        }
    }

    /// Extension-provided infos first, then exact recorded metadata, then the first
    /// recorded directory containing the path.
    pub fn find(&self, path: &Path, extra: &[(PathBuf, SourceInfo)]) -> Option<SourceInfo> {
        let display = path.display().to_string();
        let native = display.starts_with(super::source_info::NATIVE_PATH_PREFIX);
        if is_synthetic_path(&display) && !native {
            return Some(self.default_for(path));
        }
        let normalized = normalize(path);
        if let Some((_, info)) = extra.iter().find(|(root, _)| normalized.starts_with(normalize(root))) {
            return Some(SourceInfo { path: display, ..info.clone() });
        }
        let exact = self.metadata.iter().find(|(known, _)| *known == normalized || known == path);
        let containing = || self.metadata.iter().find(|(known, _)| normalized.starts_with(normalize(known)));
        exact.or_else(containing).map(|(_, metadata)| create_source_info(&display, metadata))
    }

    /// The source info of a path no settings, package, or extension named.
    pub fn default_for(&self, path: &Path) -> SourceInfo {
        let display = path.display().to_string();
        if let Some(source) = synthetic_path_source(&display) {
            return SourceInfo {
                path: display,
                source,
                scope: SourceScope::Temporary,
                origin: SourceOrigin::TopLevel,
                base_dir: None,
            };
        }
        let normalized = normalize(path);
        let kinds = ["skills", "prompts", "themes", "extensions"];
        let project = self.cwd.join(CONFIG_DIR_NAME);
        let roots = [(&self.agent_dir, SourceScope::User), (&project, SourceScope::Project)];
        for (base, scope) in roots {
            for kind in kinds {
                let root = base.join(kind);
                if is_under_path(&normalized, &root) {
                    let base_dir = Some(root.display().to_string());
                    return SourceInfo {
                        path: display,
                        source: "local".into(),
                        scope,
                        origin: SourceOrigin::TopLevel,
                        base_dir,
                    };
                }
            }
        }
        let base_dir =
            if normalized.is_dir() { normalized.clone() } else { normalized.parent().unwrap_or(&normalized).into() };
        SourceInfo {
            path: display,
            source: "local".into(),
            scope: SourceScope::Temporary,
            origin: SourceOrigin::TopLevel,
            base_dir: Some(base_dir.display().to_string()),
        }
    }

    pub fn info_for(&self, path: &Path, extra: &[(PathBuf, SourceInfo)]) -> SourceInfo {
        self.find(path, extra).unwrap_or_else(|| self.default_for(path))
    }
}

/// Resolve each path against `cwd` (synthetic paths stay) and drop canonical duplicates.
pub fn merge_paths(cwd: &Path, primary: &[PathBuf], additional: &[String]) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    let resolved = primary.iter().map(|path| resolve_resource_path(cwd, &path.to_string_lossy()));
    let extra = additional.iter().map(|path| resolve_resource_path(cwd, path));
    resolved.chain(extra).filter(|path| seen.insert(canonicalize(path))).collect()
}

pub fn resolve_resource_path(cwd: &Path, path: &str) -> PathBuf {
    if is_synthetic_path(path) { PathBuf::from(path) } else { resolve_path(path, cwd) }
}
