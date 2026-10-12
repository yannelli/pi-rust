//! Resource discovery from settings entries and conventional directories, with the `!`,
//! `+`, and `-` filters. Ported from the local-resource parts of upstream `core/package-manager.ts`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::brand::CONFIG_DIR_NAME;
use super::paths::{IgnoreRules, canonicalize, file_kind, relative_posix, resolve_path, sorted_entries};
use super::source_info::{
    BUILTIN_PATH_PREFIX, PathMetadata, ResolvedPaths, ResolvedResource, SourceOrigin, SourceScope,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ResourceType {
    Extensions,
    Skills,
    Prompts,
    Themes,
}

impl ResourceType {
    pub const ALL: [Self; 4] = [Self::Extensions, Self::Skills, Self::Prompts, Self::Themes];

    /// The conventional directory name, which is also the settings key.
    pub fn dir_name(self) -> &'static str {
        match self {
            Self::Extensions => "extensions",
            Self::Skills => "skills",
            Self::Prompts => "prompts",
            Self::Themes => "themes",
        }
    }

    fn matches_file(self, name: &str) -> bool {
        match self {
            Self::Extensions => name.ends_with(".ts") || name.ends_with(".js"),
            Self::Skills | Self::Prompts => name.ends_with(".md"),
            Self::Themes => name.ends_with(".json"),
        }
    }
}

/// The `extensions`, `skills`, `prompts`, and `themes` entries of one settings file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourceSettings {
    pub extensions: Vec<String>,
    pub skills: Vec<String>,
    pub prompts: Vec<String>,
    pub themes: Vec<String>,
}

impl ResourceSettings {
    pub fn entries(&self, kind: ResourceType) -> &[String] {
        match kind {
            ResourceType::Extensions => &self.extensions,
            ResourceType::Skills => &self.skills,
            ResourceType::Prompts => &self.prompts,
            ResourceType::Themes => &self.themes,
        }
    }
}

/// `pi` loads root `.md` files of a skills directory; `agents` (`.agents/skills`) loads
/// `.md` files only below the root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkillDiscoveryMode {
    Pi,
    Agents,
}

fn is_hidden(name: &str) -> bool {
    name.starts_with('.') || name == "node_modules"
}

/// Skill files under `dir`: a `SKILL.md` ends the search in its directory.
pub fn collect_skill_entries(dir: &Path, mode: SkillDiscoveryMode) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if dir.exists() {
        collect_skills_in(dir, mode, &mut IgnoreRules::new(dir), dir, &mut out);
    }
    out
}

fn collect_skills_in(
    dir: &Path,
    mode: SkillDiscoveryMode,
    rules: &mut IgnoreRules,
    root: &Path,
    out: &mut Vec<PathBuf>,
) {
    rules.add_dir(dir);
    let entries = sorted_entries(dir);
    let skill_file = dir.join("SKILL.md");
    if entries.iter().any(|(name, _)| name == "SKILL.md")
        && file_kind(&skill_file).is_some_and(|(is_file, _)| is_file)
        && !rules.ignores(&relative_posix(root, &skill_file), false)
    {
        out.push(skill_file);
        return;
    }
    for (name, path) in entries {
        if is_hidden(&name) {
            continue;
        }
        let Some((is_file, is_dir)) = file_kind(&path) else { continue };
        let relative = relative_posix(root, &path);
        let at_root = dir == root;
        let wanted_here = match mode {
            SkillDiscoveryMode::Pi => at_root,
            SkillDiscoveryMode::Agents => !at_root,
        };
        if is_file && name.ends_with(".md") && wanted_here && !rules.ignores(&relative, false) {
            out.push(path);
        } else if is_dir && !rules.ignores(&relative, true) {
            collect_skills_in(&path, mode, rules, root, out);
        }
    }
}

/// Files of `kind` anywhere under `dir`, skipping hidden entries and ignored paths.
pub fn collect_files(dir: &Path, kind: ResourceType) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if dir.exists() {
        collect_files_in(dir, kind, &mut IgnoreRules::new(dir), dir, &mut out);
    }
    out
}

fn collect_files_in(dir: &Path, kind: ResourceType, rules: &mut IgnoreRules, root: &Path, out: &mut Vec<PathBuf>) {
    rules.add_dir(dir);
    for (name, path) in sorted_entries(dir) {
        let Some((is_file, is_dir)) = file_kind(&path).filter(|_| !is_hidden(&name)) else { continue };
        if rules.ignores(&relative_posix(root, &path), is_dir) {
            continue;
        }
        if is_dir {
            collect_files_in(&path, kind, rules, root, out);
        } else if is_file && kind.matches_file(&name) {
            out.push(path);
        }
    }
}

/// Direct children of `dir` that are files of `kind` (prompt and theme directories).
pub fn collect_direct_files(dir: &Path, kind: ResourceType) -> Vec<PathBuf> {
    let mut rules = IgnoreRules::new(dir);
    rules.add_dir(dir);
    sorted_entries(dir)
        .into_iter()
        .filter(|(name, path)| {
            !is_hidden(name)
                && kind.matches_file(name)
                && file_kind(path).is_some_and(|(is_file, _)| is_file)
                && !rules.ignores(&relative_posix(dir, path), false)
        })
        .map(|(_, path)| path)
        .collect()
}

/// The entry files of an extension directory: `package.json` `pi.extensions`, else `index.ts` or `index.js`.
pub fn resolve_extension_entries(dir: &Path) -> Option<Vec<PathBuf>> {
    let manifest = std::fs::read_to_string(dir.join("package.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(text.trim_start_matches('\u{feff}')).ok());
    let declared = manifest.as_ref().and_then(|value| value.pointer("/pi/extensions")?.as_array().cloned());
    let entries: Vec<PathBuf> = declared
        .unwrap_or_default()
        .iter()
        .filter_map(|entry| entry.as_str())
        .map(|entry| super::paths::normalize(&dir.join(entry)))
        .filter(|path| path.exists())
        .collect();
    if !entries.is_empty() {
        return Some(entries);
    }
    ["index.ts", "index.js"].iter().map(|name| dir.join(name)).find(|path| path.exists()).map(|path| vec![path])
}

/// Extensions in a directory: its own entry files, or each `.ts`/`.js` file and extension subdirectory.
pub fn collect_extension_entries(dir: &Path) -> Vec<PathBuf> {
    if !dir.exists() {
        return Vec::new();
    }
    if let Some(entries) = resolve_extension_entries(dir) {
        return entries;
    }
    let mut rules = IgnoreRules::new(dir);
    rules.add_dir(dir);
    let mut out = Vec::new();
    for (name, path) in sorted_entries(dir) {
        let Some((is_file, is_dir)) = file_kind(&path).filter(|_| !is_hidden(&name)) else { continue };
        if rules.ignores(&relative_posix(dir, &path), is_dir) {
            continue;
        }
        if is_file && ResourceType::Extensions.matches_file(&name) {
            out.push(path);
        } else if is_dir {
            out.extend(resolve_extension_entries(&path).unwrap_or_default());
        }
    }
    out
}

/// Files of `kind` in a directory named by settings or a package.
pub fn collect_resource_files(dir: &Path, kind: ResourceType) -> Vec<PathBuf> {
    match kind {
        ResourceType::Skills => collect_skill_entries(dir, SkillDiscoveryMode::Pi),
        ResourceType::Extensions => collect_extension_entries(dir),
        _ => collect_files(dir, kind),
    }
}

fn collect_files_from_paths(paths: &[PathBuf], kind: ResourceType) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for path in paths {
        match file_kind(path) {
            Some((true, _)) => files.push(path.clone()),
            Some((_, true)) => files.extend(collect_resource_files(path, kind)),
            _ => {}
        }
    }
    files
}

/// The nearest ancestor of `start` (inclusive) holding `.git`.
pub fn find_git_repo_root(start: &Path) -> Option<PathBuf> {
    start.ancestors().find(|dir| dir.join(".git").exists()).map(Path::to_path_buf)
}

/// `.agents/skills` in `start` and each ancestor, stopping at the repository root.
pub fn ancestor_agents_skill_dirs(start: &Path) -> Vec<PathBuf> {
    let repo_root = find_git_repo_root(start);
    let mut dirs = Vec::new();
    for dir in start.ancestors() {
        dirs.push(dir.join(".agents").join("skills"));
        if repo_root.as_deref() == Some(dir) {
            break;
        }
    }
    dirs
}

pub use super::filters::{apply_patterns, is_enabled_by_overrides, is_pattern};

/// Resources by type in discovery order; the first entry for a path keeps its metadata.
#[derive(Default)]
pub struct ResourceAccumulator {
    lists: [Vec<ResolvedResource>; 4],
    seen: [HashSet<PathBuf>; 4],
}

impl ResourceAccumulator {
    fn index(kind: ResourceType) -> usize {
        ResourceType::ALL.iter().position(|candidate| *candidate == kind).unwrap_or_default()
    }

    pub fn add(&mut self, kind: ResourceType, path: PathBuf, metadata: PathMetadata, enabled: bool) {
        let index = Self::index(kind);
        if self.seen[index].insert(path.clone()) {
            self.lists[index].push(ResolvedResource { path, enabled, metadata });
        }
    }

    pub fn add_all(&mut self, kind: ResourceType, resources: &[ResolvedResource]) {
        for resource in resources {
            self.add(kind, resource.path.clone(), resource.metadata.clone(), resource.enabled);
        }
    }

    /// Sorted by precedence and deduplicated by canonical path.
    pub fn into_resolved(self) -> ResolvedPaths {
        let [extensions, skills, prompts, themes] = self.lists.map(|mut list| {
            list.sort_by_key(|resource| resource.metadata.precedence_rank());
            let mut seen = HashSet::new();
            list.retain(|resource| seen.insert(canonicalize(&resource.path)));
            list
        });
        ResolvedPaths { extensions, skills, prompts, themes }
    }
}

/// Inputs of [`resolve_local_resources`].
#[derive(Clone, Debug)]
pub struct LocalResourceOptions<'a> {
    pub cwd: &'a Path,
    pub agent_dir: &'a Path,
    pub home: &'a Path,
    pub project_trusted: bool,
    pub user: &'a ResourceSettings,
    /// Ignored unless the project is trusted.
    pub project: &'a ResourceSettings,
    /// Names of built-in extensions, listed as `builtin:<name>` paths.
    pub builtin_extensions: &'a [String],
}

fn resolve_local_entries(
    accumulator: &mut ResourceAccumulator,
    kind: ResourceType,
    entries: &[String],
    metadata: &PathMetadata,
    base_dir: &Path,
) {
    let (patterns, plain): (Vec<String>, Vec<String>) = entries.iter().cloned().partition(|entry| is_pattern(entry));
    let resolved: Vec<PathBuf> = plain.iter().map(|entry| resolve_path(entry, base_dir)).collect();
    let files = collect_files_from_paths(&resolved, kind);
    let enabled = apply_patterns(&files, &patterns, base_dir);
    for file in files {
        let is_enabled = enabled.contains(&file);
        accumulator.add(kind, file, metadata.clone(), is_enabled);
    }
}

/// Settings entries, then auto-discovered directories, then built-in extensions, merged after
/// `packages` (resolved by the package manager). Untrusted projects contribute nothing.
pub fn resolve_local_resources(options: &LocalResourceOptions<'_>, packages: &ResolvedPaths) -> ResolvedPaths {
    let mut acc = ResourceAccumulator::default();
    let keep =
        |resource: &&ResolvedResource| options.project_trusted || resource.metadata.scope != SourceScope::Project;
    for (kind, list) in
        ResourceType::ALL.into_iter().zip([&packages.extensions, &packages.skills, &packages.prompts, &packages.themes])
    {
        let kept: Vec<ResolvedResource> = list.iter().filter(keep).cloned().collect();
        acc.add_all(kind, &kept);
    }
    let empty = ResourceSettings::default();
    let project = if options.project_trusted { options.project } else { &empty };
    let user_base = options.agent_dir.to_path_buf();
    let project_base = options.cwd.join(CONFIG_DIR_NAME);
    let local = |scope| PathMetadata::new("local", scope, SourceOrigin::TopLevel);
    for kind in ResourceType::ALL {
        resolve_local_entries(&mut acc, kind, project.entries(kind), &local(SourceScope::Project), &project_base);
        resolve_local_entries(&mut acc, kind, options.user.entries(kind), &local(SourceScope::User), &user_base);
    }
    let auto = |scope, base: &Path| PathMetadata::new("auto", scope, SourceOrigin::TopLevel).with_base_dir(base);
    let mut add =
        |kind: ResourceType, paths: Vec<PathBuf>, metadata: PathMetadata, overrides: &[String], base: &Path| {
            for path in paths {
                let enabled = is_enabled_by_overrides(&path, overrides, base);
                acc.add(kind, path, metadata.clone(), enabled);
            }
        };
    let user_agents_skills = options.home.join(".agents").join("skills");
    let project_meta = auto(SourceScope::Project, &project_base);
    let user_meta = auto(SourceScope::User, &user_base);
    if options.project_trusted {
        let dir = |kind: ResourceType| project_base.join(kind.dir_name());
        let skills_overrides = project.entries(ResourceType::Skills);
        add(
            ResourceType::Extensions,
            collect_extension_entries(&dir(ResourceType::Extensions)),
            project_meta.clone(),
            project.entries(ResourceType::Extensions),
            &project_base,
        );
        let skills = collect_skill_entries(&dir(ResourceType::Skills), SkillDiscoveryMode::Pi);
        add(ResourceType::Skills, skills, project_meta.clone(), skills_overrides, &project_base);
        for agents_skills in ancestor_agents_skill_dirs(options.cwd) {
            if super::paths::normalize(&agents_skills) == super::paths::normalize(&user_agents_skills) {
                continue;
            }
            let agents_base = agents_skills.parent().map(Path::to_path_buf).unwrap_or_default();
            let entries = collect_skill_entries(&agents_skills, SkillDiscoveryMode::Agents);
            let meta = auto(SourceScope::Project, &agents_base);
            add(ResourceType::Skills, entries, meta, skills_overrides, &agents_base);
        }
        for kind in [ResourceType::Prompts, ResourceType::Themes] {
            let files = collect_direct_files(&dir(kind), kind);
            add(kind, files, project_meta.clone(), project.entries(kind), &project_base);
        }
    }
    let user_dir = |kind: ResourceType| user_base.join(kind.dir_name());
    let user = options.user;
    let extensions = collect_extension_entries(&user_dir(ResourceType::Extensions));
    add(ResourceType::Extensions, extensions, user_meta.clone(), user.entries(ResourceType::Extensions), &user_base);
    let skills = collect_skill_entries(&user_dir(ResourceType::Skills), SkillDiscoveryMode::Pi);
    add(ResourceType::Skills, skills, user_meta.clone(), user.entries(ResourceType::Skills), &user_base);
    let agents_base = options.home.join(".agents");
    let agents_skills = collect_skill_entries(&user_agents_skills, SkillDiscoveryMode::Agents);
    let agents_meta = auto(SourceScope::User, &agents_base);
    add(ResourceType::Skills, agents_skills, agents_meta, user.entries(ResourceType::Skills), &agents_base);
    for kind in [ResourceType::Prompts, ResourceType::Themes] {
        let files = collect_direct_files(&user_dir(kind), kind);
        add(kind, files, user_meta.clone(), user.entries(kind), &user_base);
    }
    for name in options.builtin_extensions {
        let path = PathBuf::from(format!("{BUILTIN_PATH_PREFIX}{name}"));
        let project_overrides: Vec<String> =
            project.extensions.iter().filter(|entry| entry.starts_with(['!', '+', '-'])).cloned().collect();
        let project_enabled = super::filters::autoload_override(&path, &project_overrides, &project_base);
        let scope = if project_enabled.is_some() { SourceScope::Project } else { SourceScope::User };
        let enabled = project_enabled.unwrap_or_else(|| is_enabled_by_overrides(&path, &user.extensions, &user_base));
        acc.add(ResourceType::Extensions, path, PathMetadata::new("builtin", scope, SourceOrigin::TopLevel), enabled);
    }
    acc.into_resolved()
}
