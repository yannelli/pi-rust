//! Data of `pi config`, ported from upstream
//! `modes/interactive/components/config-selector.ts`: resolved resources grouped by
//! package or settings location. [`super::ConfigEditor`] holds the toggle rules.

use std::path::{Path, PathBuf};

use crate::resources::brand::CONFIG_DIR_NAME;
use crate::resources::source_info::BUILTIN_PATH_PREFIX;
use crate::resources::{PathMetadata, ResolvedPaths, ResolvedResource, ResourceType, SourceOrigin, SourceScope};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigWriteScope {
    Global,
    Project,
}

/// Resolved resources as seen from global settings only and with project settings.
#[derive(Clone, Debug, Default)]
pub struct ScopedResolvedPaths {
    pub global: ResolvedPaths,
    pub project: ResolvedPaths,
}

/// A project-mode entry: inherit the global state, force load (`+`), or force unload (`-`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectOverrideState {
    Inherit,
    Load,
    Unload,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigItem {
    pub path: PathBuf,
    pub enabled: bool,
    pub metadata: PathMetadata,
    pub kind: ResourceType,
    pub display_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigSubgroup {
    pub kind: ResourceType,
    pub label: &'static str,
    pub items: Vec<ConfigItem>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigGroup {
    pub key: String,
    pub label: String,
    pub scope: SourceScope,
    pub origin: SourceOrigin,
    pub source: String,
    pub subgroups: Vec<ConfigSubgroup>,
}

pub fn type_label(kind: ResourceType) -> &'static str {
    match kind {
        ResourceType::Extensions => "Extensions",
        ResourceType::Skills => "Skills",
        ResourceType::Prompts => "Prompts",
        ResourceType::Themes => "Themes",
    }
}

/// A directory for display: `~` for the home directory, forward slashes, trailing `/`.
pub fn format_base_dir(base: &Path, home: &Path) -> String {
    let text = base.display().to_string();
    let home_text = home.display().to_string();
    let display = match text.strip_prefix(&home_text) {
        Some("") => "~".to_string(),
        Some(rest) => format!("~{}", rest.replace('\\', "/")),
        None => text.replace('\\', "/"),
    };
    if display.ends_with('/') { display } else { format!("{display}/") }
}

fn group_label(metadata: &PathMetadata, agent_dir: &Path, home: &Path) -> String {
    let user = metadata.scope == SourceScope::User;
    let scope_name = match metadata.scope {
        SourceScope::User => "user",
        SourceScope::Project => "project",
        SourceScope::Temporary => "temporary",
    };
    if metadata.origin == SourceOrigin::Package {
        return format!("{} ({scope_name})", metadata.source);
    }
    if metadata.source == "builtin" {
        return if user { "Built-in".into() } else { "Built-in (project override)".into() };
    }
    if metadata.source == "auto" {
        return match (&metadata.base_dir, user) {
            (Some(base), true) => format!("User ({})", format_base_dir(base, home)),
            (Some(base), false) => format!("Project ({})", format_base_dir(base, home)),
            (None, true) => format!("User ({})", format_base_dir(agent_dir, home)),
            (None, false) => format!("Project ({CONFIG_DIR_NAME}/)"),
        };
    }
    if user { "User settings".into() } else { "Project settings".into() }
}

fn display_name(resource: &ResolvedResource, kind: ResourceType) -> String {
    let path = &resource.path;
    let name = |path: Option<&Path>| path.and_then(Path::file_name).map(|n| n.to_string_lossy().into_owned());
    let file = name(Some(path)).unwrap_or_default();
    let parent = name(path.parent()).unwrap_or_default();
    if resource.metadata.source == "builtin" {
        path.display().to_string().trim_start_matches(BUILTIN_PATH_PREFIX).to_string()
    } else if path.to_string_lossy().starts_with(crate::resources::source_info::NATIVE_PATH_PREFIX) {
        path.display().to_string()
    } else if kind == ResourceType::Extensions && parent != "extensions" {
        format!("{parent}/{file}")
    } else if kind == ResourceType::Skills && file == "SKILL.md" {
        parent
    } else {
        file
    }
}

fn compare_text(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| a.cmp(b))
}

fn group_index(groups: &mut Vec<ConfigGroup>, metadata: &PathMetadata, agent_dir: &Path, home: &Path) -> usize {
    let base = metadata.base_dir.as_ref().map(|dir| dir.display().to_string()).unwrap_or_default();
    let key = format!("{:?}:{:?}:{}:{base}", metadata.origin, metadata.scope, metadata.source);
    if let Some(index) = groups.iter().position(|group| group.key == key) {
        return index;
    }
    groups.push(ConfigGroup {
        key,
        label: group_label(metadata, agent_dir, home),
        scope: metadata.scope,
        origin: metadata.origin,
        source: metadata.source.clone(),
        subgroups: Vec::new(),
    });
    groups.len() - 1
}

/// Groups by origin, scope, source, and base directory: packages first, user before project;
/// subgroups in type order and items by display name.
pub fn build_config_groups(resolved: &ResolvedPaths, agent_dir: &Path, home: &Path) -> Vec<ConfigGroup> {
    let mut groups: Vec<ConfigGroup> = Vec::new();
    let lists = [&resolved.extensions, &resolved.skills, &resolved.prompts, &resolved.themes];
    for (kind, list) in ResourceType::ALL.into_iter().zip(lists) {
        for resource in list {
            let index = group_index(&mut groups, &resource.metadata, agent_dir, home);
            let subgroups = &mut groups[index].subgroups;
            let position = match subgroups.iter().position(|subgroup| subgroup.kind == kind) {
                Some(position) => position,
                None => {
                    subgroups.push(ConfigSubgroup { kind, label: type_label(kind), items: Vec::new() });
                    subgroups.len() - 1
                }
            };
            subgroups[position].items.push(ConfigItem {
                path: resource.path.clone(),
                enabled: resource.enabled,
                metadata: resource.metadata.clone(),
                kind,
                display_name: display_name(resource, kind),
            });
        }
    }
    let rank = |scope: SourceScope| u8::from(scope != SourceScope::User);
    groups.sort_by(|a, b| {
        (a.origin != SourceOrigin::Package)
            .cmp(&(b.origin != SourceOrigin::Package))
            .then(rank(a.scope).cmp(&rank(b.scope)))
            .then_with(|| compare_text(&a.source, &b.source))
    });
    for group in &mut groups {
        group.subgroups.sort_by_key(|subgroup| ResourceType::ALL.iter().position(|kind| *kind == subgroup.kind));
        for subgroup in &mut group.subgroups {
            subgroup.items.sort_by(|a, b| compare_text(&a.display_name, &b.display_name));
        }
    }
    groups
}
