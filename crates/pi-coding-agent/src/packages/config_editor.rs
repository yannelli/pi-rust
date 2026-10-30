//! Toggle rules of `pi config`. Global mode flips user-scope items with `+path` and
//! `-path` entries; project mode cycles inherit, load, and unload overrides in
//! `.pi/settings.json`, using `autoload: false` package entries as deltas.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::config::{
    ConfigGroup, ConfigItem, ConfigWriteScope, ProjectOverrideState, ScopedResolvedPaths, build_config_groups,
};
use super::settings_ops::{package_source_text, relative_or_absolute};
use crate::resources::brand::CONFIG_DIR_NAME;
use crate::resources::paths::{canonicalize, is_local_path, resolve_path};
use crate::resources::{ResourceType, SourceOrigin, SourceScope};
use crate::settings::schema::{PackageFilter, PackageSource};
use crate::settings::{ResourceKind, SettingsManager};

fn resource_kind(kind: ResourceType) -> ResourceKind {
    match kind {
        ResourceType::Extensions => ResourceKind::Extensions,
        ResourceType::Skills => ResourceKind::Skills,
        ResourceType::Prompts => ResourceKind::Prompts,
        ResourceType::Themes => ResourceKind::Themes,
    }
}

fn strip_override(entry: &str) -> &str {
    entry.strip_prefix(['!', '+', '-']).unwrap_or(entry)
}

fn is_native(item: &ConfigItem) -> bool {
    item.path.to_string_lossy().starts_with(crate::resources::source_info::NATIVE_PATH_PREFIX)
}

fn is_override(entry: &str) -> bool {
    entry.starts_with(['!', '+', '-'])
}

fn filter_slot(filter: &mut PackageFilter, kind: ResourceType) -> &mut Option<Vec<String>> {
    match kind {
        ResourceType::Extensions => &mut filter.extensions,
        ResourceType::Skills => &mut filter.skills,
        ResourceType::Prompts => &mut filter.prompts,
        ResourceType::Themes => &mut filter.themes,
    }
}

fn has_resource_filters(filter: &PackageFilter) -> bool {
    [&filter.extensions, &filter.skills, &filter.prompts, &filter.themes].iter().any(|slot| slot.is_some())
}

fn as_filter(package: PackageSource) -> PackageFilter {
    match package {
        PackageSource::Source(source) => PackageFilter { source, ..PackageFilter::default() },
        PackageSource::Filtered(filter) => filter,
    }
}

fn item_key(item: &ConfigItem) -> String {
    format!("{:?}:{}", item.kind, canonicalize(&item.path).display())
}

fn sign(load: bool) -> char {
    if load { '+' } else { '-' }
}

fn state_from_entries(entries: &[String], patterns: &[String], empty_is_unload: bool) -> ProjectOverrideState {
    if entries.is_empty() && empty_is_unload {
        return ProjectOverrideState::Unload;
    }
    let mut state = ProjectOverrideState::Inherit;
    for entry in entries.iter().filter(|entry| patterns.iter().any(|pattern| pattern == strip_override(entry))) {
        state = if entry.starts_with(['!', '-']) { ProjectOverrideState::Unload } else { ProjectOverrideState::Load };
    }
    state
}

/// The settings side of `pi config`: which items may toggle and what each toggle writes.
pub struct ConfigEditor {
    settings: Arc<SettingsManager>,
    cwd: PathBuf,
    agent_dir: PathBuf,
    write_scope: ConfigWriteScope,
    global_groups: Vec<ConfigGroup>,
    project_groups: Vec<ConfigGroup>,
    inherited_enabled: HashMap<String, bool>,
}

impl ConfigEditor {
    pub fn new(
        resolved: &ScopedResolvedPaths,
        settings: Arc<SettingsManager>,
        cwd: &Path,
        agent_dir: &Path,
        home: &Path,
        write_scope: ConfigWriteScope,
    ) -> Self {
        let global_groups = build_config_groups(&resolved.global, agent_dir, home);
        let project_groups = build_config_groups(&resolved.project, agent_dir, home);
        let inherited_enabled = global_groups
            .iter()
            .flat_map(|group| &group.subgroups)
            .flat_map(|subgroup| &subgroup.items)
            .map(|item| (item_key(item), item.enabled))
            .collect();
        let (cwd, agent_dir) = (cwd.to_path_buf(), agent_dir.to_path_buf());
        Self { settings, cwd, agent_dir, write_scope, global_groups, project_groups, inherited_enabled }
    }

    pub fn write_scope(&self) -> ConfigWriteScope {
        self.write_scope
    }

    /// The settings that toggles write to.
    pub fn settings(&self) -> &Arc<SettingsManager> {
        &self.settings
    }

    /// Tab in the selector switches between global and project mode.
    pub fn set_write_scope(&mut self, scope: ConfigWriteScope) {
        self.write_scope = scope;
    }

    pub fn groups(&self) -> &[ConfigGroup] {
        match self.write_scope {
            ConfigWriteScope::Global => &self.global_groups,
            ConfigWriteScope::Project => &self.project_groups,
        }
    }

    fn item_scope(item: &ConfigItem) -> SourceScope {
        if item.metadata.scope == SourceScope::Project { SourceScope::Project } else { SourceScope::User }
    }

    fn base_dir(&self, scope: SourceScope) -> PathBuf {
        if scope == SourceScope::Project { self.cwd.join(CONFIG_DIR_NAME) } else { self.agent_dir.clone() }
    }

    /// Project mode toggles any item; global mode only user-scope items.
    pub fn can_toggle(&self, item: &ConfigItem) -> bool {
        self.write_scope == ConfigWriteScope::Project || Self::item_scope(item) == SourceScope::User
    }

    /// Toggle the item at `(group, subgroup, item)` of [`Self::groups`]; returns its new
    /// enabled state, or `None` when nothing changed.
    pub fn toggle(&mut self, group: usize, subgroup: usize, item: usize) -> Option<bool> {
        let current = self.groups().get(group)?.subgroups.get(subgroup)?.items.get(item)?.clone();
        if !self.can_toggle(&current) {
            return None;
        }
        let enabled = self.toggle_resource(&current)?;
        let groups = match self.write_scope {
            ConfigWriteScope::Global => &mut self.global_groups,
            ConfigWriteScope::Project => &mut self.project_groups,
        };
        groups[group].subgroups[subgroup].items[item].enabled = enabled;
        Some(enabled)
    }

    /// Set one extension by its exact path or unambiguous display name.
    pub fn set_extension_enabled(&mut self, reference: &str, enabled: bool) -> Result<(), String> {
        let matches: Vec<_> = self
            .groups()
            .iter()
            .enumerate()
            .flat_map(|(g, group)| {
                group.subgroups.iter().enumerate().flat_map(move |(s, subgroup)| {
                    subgroup.items.iter().enumerate().filter_map(move |(i, item)| {
                        (item.kind == ResourceType::Extensions
                            && (item.path.to_string_lossy() == reference || item.display_name == reference))
                            .then_some((g, s, i, item.clone()))
                    })
                })
            })
            .collect();
        let [(g, s, i, item)] = matches.as_slice() else {
            return Err(if matches.is_empty() {
                format!("Extension not found: {reference}. Use pi extensions list or install it first.")
            } else {
                format!("Ambiguous extension: {reference}. Use the full path from pi extensions list.")
            });
        };
        if !self.can_toggle(item) {
            return Err("Use --local to change a project extension.".into());
        }
        if self.write_scope == ConfigWriteScope::Project {
            let state = if enabled { ProjectOverrideState::Load } else { ProjectOverrideState::Unload };
            if !self.set_project_override(item, state) {
                return Err(format!("Could not update extension: {reference}"));
            }
            self.project_groups[*g].subgroups[*s].items[*i].enabled = enabled;
        } else if item.enabled != enabled && self.toggle(*g, *s, *i).is_none() {
            return Err(format!("Could not update extension: {reference}"));
        }
        Ok(())
    }

    fn toggle_resource(&self, item: &ConfigItem) -> Option<bool> {
        if self.write_scope == ConfigWriteScope::Project {
            let state = self.next_override_state(item);
            if !self.set_project_override(item, state) {
                return None;
            }
            return Some(match state {
                ProjectOverrideState::Inherit => self.inherited_enabled(item),
                state => state == ProjectOverrideState::Load,
            });
        }
        let enabled = !item.enabled;
        let scope = Self::item_scope(item);
        if item.metadata.origin == SourceOrigin::TopLevel {
            let pattern = self.resource_pattern(item);
            let mut entries = self.top_level_entries(item.kind, scope);
            entries.retain(|entry| strip_override(entry) != pattern);
            entries.push(format!("{}{pattern}", sign(enabled)));
            self.set_top_level_entries(item.kind, scope, &entries);
            return Some(enabled);
        }
        let mut packages = self.packages(scope);
        let index = packages.iter().position(|package| package_source_text(package) == item.metadata.source)?;
        let mut filter = as_filter(packages[index].clone());
        let pattern = Self::package_pattern(item);
        let slot = filter_slot(&mut filter, item.kind);
        let mut entries = slot.take().unwrap_or_default();
        entries.retain(|entry| strip_override(entry) != pattern);
        entries.push(format!("{}{pattern}", sign(enabled)));
        *slot = Some(entries);
        packages[index] = PackageSource::Filtered(filter);
        self.set_packages(scope, &packages);
        Some(enabled)
    }

    fn resource_pattern(&self, item: &ConfigItem) -> String {
        if item.metadata.source == "builtin" || is_native(item) {
            return item.path.display().to_string();
        }
        let base = item.metadata.base_dir.clone().unwrap_or_else(|| self.base_dir(Self::item_scope(item)));
        relative_or_absolute(&base, &item.path)
    }

    fn package_pattern(item: &ConfigItem) -> String {
        if is_native(item) {
            return item.path.display().to_string();
        }
        let fallback = item.path.parent().map(Path::to_path_buf).unwrap_or_default();
        relative_or_absolute(item.metadata.base_dir.as_ref().unwrap_or(&fallback), &item.path)
    }

    fn top_level_entries(&self, kind: ResourceType, scope: SourceScope) -> Vec<String> {
        self.settings.resource_paths(resource_kind(kind), super::settings_scope(scope))
    }

    fn set_top_level_entries(&self, kind: ResourceType, scope: SourceScope, entries: &[String]) {
        if scope == SourceScope::Project {
            let _ = self.settings.set_project_resource_paths(resource_kind(kind), entries);
        } else {
            self.settings.set_resource_paths(resource_kind(kind), entries);
        }
    }

    fn packages(&self, scope: SourceScope) -> Vec<PackageSource> {
        self.settings.packages(super::settings_scope(scope))
    }

    fn set_packages(&self, scope: SourceScope, packages: &[PackageSource]) {
        if scope == SourceScope::Project {
            let _ = self.settings.set_project_packages(packages);
        } else {
            self.settings.set_packages(packages);
        }
    }

    /// The project override of an item; always `Inherit` in global mode.
    pub fn override_state(&self, item: &ConfigItem) -> ProjectOverrideState {
        if self.write_scope != ConfigWriteScope::Project {
            return ProjectOverrideState::Inherit;
        }
        if item.metadata.origin == SourceOrigin::TopLevel {
            let entries = self.top_level_entries(item.kind, SourceScope::Project);
            return state_from_entries(&entries, &self.top_level_override_patterns(item), false);
        }
        let Some(PackageSource::Filtered(mut filter)) = self.matching_package(item) else {
            return ProjectOverrideState::Inherit;
        };
        let autoload = filter.autoload != Some(false);
        match filter_slot(&mut filter, item.kind) {
            Some(entries) => state_from_entries(entries, &[Self::package_pattern(item)], autoload),
            None => ProjectOverrideState::Inherit,
        }
    }

    fn next_override_state(&self, item: &ConfigItem) -> ProjectOverrideState {
        match (self.override_state(item), self.inherited_enabled(item)) {
            (ProjectOverrideState::Inherit, true) | (ProjectOverrideState::Load, false) => ProjectOverrideState::Unload,
            (ProjectOverrideState::Inherit, false) | (ProjectOverrideState::Unload, true) => ProjectOverrideState::Load,
            _ => ProjectOverrideState::Inherit,
        }
    }

    fn inherited_enabled(&self, item: &ConfigItem) -> bool {
        let fallback = if Self::item_scope(item) == SourceScope::User { item.enabled } else { true };
        self.inherited_enabled.get(&item_key(item)).copied().unwrap_or(fallback)
    }

    /// User-scope items and items that also resolve from global settings alone.
    pub fn is_inherited_global_item(&self, item: &ConfigItem) -> bool {
        Self::item_scope(item) == SourceScope::User || self.inherited_enabled.contains_key(&item_key(item))
    }

    /// `[x]`, `[ ]`, or in project mode `[+]` and `[-]` for overrides.
    pub fn checkbox(&self, item: &ConfigItem) -> &'static str {
        match self.override_state(item) {
            ProjectOverrideState::Load => "[+]",
            ProjectOverrideState::Unload => "[-]",
            ProjectOverrideState::Inherit if item.enabled => "[x]",
            ProjectOverrideState::Inherit => "[ ]",
        }
    }

    /// The project-mode note after an item: `project load`, `project unload`, or `inherited global`.
    pub fn item_suffix(&self, item: &ConfigItem) -> &'static str {
        if self.write_scope != ConfigWriteScope::Project {
            return "";
        }
        match self.override_state(item) {
            ProjectOverrideState::Load => "project load",
            ProjectOverrideState::Unload => "project unload",
            ProjectOverrideState::Inherit if self.is_inherited_global_item(item) => "inherited global",
            ProjectOverrideState::Inherit => "",
        }
    }

    fn pattern_for_scope(&self, item: &ConfigItem, scope: SourceScope) -> String {
        if scope != Self::item_scope(item) || item.metadata.source == "builtin" {
            return item.path.display().to_string();
        }
        self.resource_pattern(item)
    }

    fn top_level_override_patterns(&self, item: &ConfigItem) -> Vec<String> {
        let mut patterns = vec![
            self.pattern_for_scope(item, SourceScope::Project),
            item.path.display().to_string(),
            relative_or_absolute(&self.base_dir(SourceScope::Project), &item.path),
        ];
        if let Some(base) = &item.metadata.base_dir {
            patterns.push(relative_or_absolute(base, &item.path));
        }
        patterns
    }

    fn sources_match(&self, left: &str, left_scope: SourceScope, right: &str) -> bool {
        if left == right {
            return true;
        }
        if !is_local_path(left) || !is_local_path(right) {
            return false;
        }
        resolve_path(left, &self.base_dir(left_scope)) == resolve_path(right, &self.base_dir(SourceScope::Project))
    }

    fn matching_index(&self, item: &ConfigItem, packages: &[PackageSource]) -> Option<usize> {
        let scope = Self::item_scope(item);
        packages
            .iter()
            .position(|package| self.sources_match(&item.metadata.source, scope, package_source_text(package)))
    }

    fn matching_package(&self, item: &ConfigItem) -> Option<PackageSource> {
        let packages = self.packages(SourceScope::Project);
        self.matching_index(item, &packages).map(|index| packages[index].clone())
    }

    fn set_project_override(&self, item: &ConfigItem, state: ProjectOverrideState) -> bool {
        if item.metadata.origin != SourceOrigin::TopLevel {
            return self.set_project_package_override(item, state);
        }
        let inherited = self.is_inherited_global_item(item);
        let pattern = if inherited {
            item.path.display().to_string()
        } else {
            self.pattern_for_scope(item, SourceScope::Project)
        };
        let patterns = self.top_level_override_patterns(item);
        let mut entries = self.top_level_entries(item.kind, SourceScope::Project);
        entries.retain(|entry| {
            let target = strip_override(entry);
            let overridden = is_override(entry) && patterns.iter().any(|pattern| pattern == target);
            !overridden && !(state == ProjectOverrideState::Inherit && inherited && target == pattern)
        });
        if state != ProjectOverrideState::Inherit {
            if inherited && item.metadata.source != "builtin" && !entries.contains(&pattern) {
                entries.push(pattern.clone());
            }
            entries.push(format!("{}{pattern}", sign(state == ProjectOverrideState::Load)));
        }
        self.set_top_level_entries(item.kind, SourceScope::Project, &entries);
        true
    }

    fn package_override_source(&self, item: &ConfigItem) -> PackageFilter {
        let source = &item.metadata.source;
        let source = if is_local_path(source) {
            let path = resolve_path(source, &self.base_dir(Self::item_scope(item)));
            relative_or_absolute(&self.base_dir(SourceScope::Project), &path)
        } else {
            source.clone()
        };
        PackageFilter { source, autoload: Some(false), ..PackageFilter::default() }
    }

    fn set_project_package_override(&self, item: &ConfigItem, state: ProjectOverrideState) -> bool {
        let mut packages = self.packages(SourceScope::Project);
        let index = match self.matching_index(item, &packages) {
            Some(index) => index,
            None if state == ProjectOverrideState::Inherit => return false,
            None => {
                packages.push(PackageSource::Filtered(self.package_override_source(item)));
                packages.len() - 1
            }
        };
        let mut filter = as_filter(packages[index].clone());
        let pattern = Self::package_pattern(item);
        let slot = filter_slot(&mut filter, item.kind);
        let mut entries = slot.take().unwrap_or_default();
        entries.retain(|entry| strip_override(entry) != pattern);
        if state != ProjectOverrideState::Inherit {
            entries.push(format!("{}{pattern}", sign(state == ProjectOverrideState::Load)));
        }
        *slot = (!entries.is_empty()).then_some(entries);
        if has_resource_filters(&filter) {
            packages[index] = PackageSource::Filtered(filter);
        } else if filter.autoload == Some(false) {
            packages.remove(index);
        } else {
            packages[index] = PackageSource::Source(filter.source);
        }
        self.set_packages(SourceScope::Project, &packages);
        true
    }
}
