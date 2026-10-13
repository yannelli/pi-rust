//! The startup listing of loaded context files, skills, prompts, and extensions, and resource
//! diagnostics (upstream `showLoadedResources` and its path helpers).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use pi_tui::{Spacer, component};

use super::components::{ExpandableText, ThemedText};
use super::mode::InteractiveMode;
use super::util::{cwd_relative_path, format_display_path, resolve_lexically};
use crate::packages::git_url::parse_git_url;
use crate::resources::source_info::{SourceInfo, SourceScope};
use crate::resources::{DiagnosticKind, ResourceDiagnostic};
use crate::theme::{ThemeColor, theme};

/// A resource path and where it came from.
#[derive(Clone)]
pub(super) struct Item {
    pub path: String,
    pub source: Option<SourceInfo>,
}

fn is_package_source(source: Option<&SourceInfo>) -> bool {
    source.is_some_and(|info| info.source.starts_with("npm:") || info.source.starts_with("git:"))
}

/// The path inside its package, or the home-relative display path.
fn short_path(full: &str, source: Option<&SourceInfo>) -> String {
    let normalized = full.replace('\\', "/");
    if let Some(info) = source.filter(|info| is_package_source(Some(info)))
        && let Some(base) = &info.base_dir
        && let Ok(relative) = Path::new(full).strip_prefix(base)
        && !relative.as_os_str().is_empty()
    {
        return relative.to_string_lossy().replace('\\', "/");
    }
    let origin = source.map(|info| info.source.as_str()).unwrap_or("");
    if origin.starts_with("npm:")
        && let Some(index) = normalized.find("node_modules/")
    {
        let rest = &normalized[index + "node_modules/".len()..];
        let skip = if rest.starts_with('@') { 2 } else { 1 };
        let parts: Vec<&str> = rest.splitn(skip + 1, '/').collect();
        if parts.len() == skip + 1 {
            return parts[skip].to_string();
        }
    }
    if origin.starts_with("git:")
        && let Some(index) = normalized.find("git/")
    {
        let parts: Vec<&str> = normalized[index + 4..].splitn(3, '/').collect();
        if parts.len() == 3 {
            return parts[2].to_string();
        }
    }
    format_display_path(full)
}

fn display_source(source: Option<&SourceInfo>) -> (String, Option<&'static str>) {
    let origin = source.map(|info| info.source.as_str()).unwrap_or("local");
    let scope = source.map(|info| info.scope).unwrap_or(SourceScope::Project);
    let scope_label = match scope {
        SourceScope::User => "user",
        SourceScope::Project => "project",
        SourceScope::Temporary => "temp",
    };
    match origin {
        "local" => match scope {
            SourceScope::User => ("user".into(), None),
            SourceScope::Project => ("project".into(), None),
            SourceScope::Temporary => ("path".into(), Some("temp")),
        },
        "cli" => ("path".into(), (scope == SourceScope::Temporary).then_some("temp")),
        other => (other.to_string(), Some(scope_label)),
    }
}

fn path_with_source(path: &str, source: Option<&SourceInfo>) -> String {
    match source {
        Some(info) => {
            let (label, scope) = display_source(Some(info));
            let label = scope.map(|scope| format!("{label} ({scope})")).unwrap_or(label);
            format!("{label} {}", short_path(path, Some(info)))
        }
        None => format_display_path(path),
    }
}

fn scope_group(source: Option<&SourceInfo>) -> &'static str {
    let origin = source.map(|info| info.source.as_str()).unwrap_or("local");
    match source.map(|info| info.scope).unwrap_or(SourceScope::Project) {
        _ if origin == "cli" => "path",
        SourceScope::Temporary => "path",
        SourceScope::User => "user",
        SourceScope::Project => "project",
    }
}

/// Items by scope (project, user, path), loose paths first, then packages by source.
fn format_scope_groups(items: &[Item], format_path: &dyn Fn(&Item) -> String) -> String {
    let t = theme();
    let mut lines = Vec::new();
    for scope in ["project", "user", "path"] {
        let in_scope: Vec<&Item> = items.iter().filter(|item| scope_group(item.source.as_ref()) == scope).collect();
        if in_scope.is_empty() {
            continue;
        }
        lines.push(format!("  {}", t.fg(ThemeColor::Accent, scope)));
        let mut paths: Vec<&Item> =
            in_scope.iter().copied().filter(|item| !is_package_source(item.source.as_ref())).collect();
        paths.sort_by(|a, b| a.path.cmp(&b.path));
        for item in paths {
            lines.push(t.fg(ThemeColor::Dim, &format!("    {}", format_path(item))));
        }
        let mut packages: BTreeMap<String, Vec<&Item>> = BTreeMap::new();
        for item in in_scope.iter().copied().filter(|item| is_package_source(item.source.as_ref())) {
            let source = item.source.as_ref().map(|info| info.source.clone()).unwrap_or_default();
            packages.entry(source).or_default().push(item);
        }
        for (source, mut entries) in packages {
            lines.push(format!("    {}", t.fg(ThemeColor::MdLink, &source)));
            entries.sort_by(|a, b| a.path.cmp(&b.path));
            for item in entries {
                let short = short_path(&item.path, item.source.as_ref());
                let shown = if format_path(item).starts_with('/') { format_path(item) } else { short };
                lines.push(t.fg(ThemeColor::Dim, &format!("      {shown}")));
            }
        }
    }
    lines.join("\n")
}

fn compact_list(items: Vec<String>, sort: bool) -> String {
    let mut labels: Vec<String> =
        items.into_iter().map(|item| item.trim().to_string()).filter(|item| !item.is_empty()).collect();
    if sort {
        labels.sort();
    }
    theme().fg(ThemeColor::Dim, &format!("  {}", labels.join(", ")))
}

/// The last path segment of an extension, or its package name.
fn compact_extension_label(item: &Item, all: &[Item]) -> String {
    if let Some(info) = item.source.as_ref().filter(|info| is_package_source(Some(info))) {
        let label = info
            .source
            .strip_prefix("npm:")
            .map(str::to_string)
            .or_else(|| parse_git_url(&info.source).map(|git| git.path))
            .unwrap_or_else(|| info.source.clone());
        let short = short_path(&item.path, Some(info));
        let inner = short.strip_prefix("extensions/").unwrap_or(&short);
        let path = Path::new(inner);
        let stem = path.file_stem().map(|stem| stem.to_string_lossy().to_string()).unwrap_or_default();
        if stem == "index" {
            let dir = path.parent().map(|dir| dir.to_string_lossy().to_string()).unwrap_or_default();
            return if dir.is_empty() || dir == "." { label } else { format!("{label}:{dir}") };
        }
        return format!("{label}:{inner}");
    }
    let segments = |path: &str| -> Vec<String> {
        let mut segments: Vec<String> = format_display_path(path)
            .replace('\\', "/")
            .split('/')
            .filter(|segment| !segment.is_empty() && *segment != "~")
            .map(String::from)
            .collect();
        if segments.len() > 1 && segments.last().is_some_and(|last| last == "index.ts" || last == "index.js") {
            segments.pop();
        }
        segments
    };
    let own = segments(&item.path);
    let others: Vec<Vec<String>> = all
        .iter()
        .filter(|other| other.path != item.path && !is_package_source(other.source.as_ref()))
        .map(|other| segments(&other.path))
        .collect();
    for count in 1..=own.len() {
        let candidate = own[own.len() - count..].join("/");
        let unique = others.iter().all(|other| other[other.len().saturating_sub(count)..].join("/") != candidate);
        if unique {
            return candidate;
        }
    }
    own.join("/")
}

/// Collisions grouped by name, then other diagnostics with their paths.
fn format_diagnostics(diagnostics: &[ResourceDiagnostic], sources: &HashMap<String, SourceInfo>) -> String {
    let t = theme();
    let find = |path: &str| -> Option<SourceInfo> {
        let mut current = path.to_string();
        loop {
            if let Some(info) = sources.get(&current) {
                return Some(info.clone());
            }
            let index = current.rfind('/')?;
            current.truncate(index);
        }
    };
    let mut lines = Vec::new();
    let mut collisions: Vec<(String, Vec<&ResourceDiagnostic>)> = Vec::new();
    for diagnostic in diagnostics {
        if let (DiagnosticKind::Collision, Some(collision)) = (diagnostic.kind, &diagnostic.collision) {
            match collisions.iter_mut().find(|(name, _)| *name == collision.name) {
                Some((_, list)) => list.push(diagnostic),
                None => collisions.push((collision.name.clone(), vec![diagnostic])),
            }
        }
    }
    for (name, list) in &collisions {
        let Some(first) = list[0].collision.as_ref() else { continue };
        lines.push(t.fg(ThemeColor::Warning, &format!("  \"{name}\" collision:")));
        let winner = path_with_source(&first.winner_path, find(&first.winner_path).as_ref());
        lines.push(t.fg(ThemeColor::Dim, &format!("    {} {winner}", t.fg(ThemeColor::Success, "✓"))));
        for diagnostic in list {
            if let Some(collision) = &diagnostic.collision {
                let loser = path_with_source(&collision.loser_path, find(&collision.loser_path).as_ref());
                lines.push(t.fg(ThemeColor::Dim, &format!("    {} {loser} (skipped)", t.fg(ThemeColor::Warning, "✗"))));
            }
        }
    }
    for diagnostic in diagnostics.iter().filter(|diagnostic| diagnostic.kind != DiagnosticKind::Collision) {
        let color = if diagnostic.kind == DiagnosticKind::Error { ThemeColor::Error } else { ThemeColor::Warning };
        match &diagnostic.path {
            Some(path) => {
                lines.push(t.fg(color, &format!("  {}", path_with_source(path, find(path).as_ref()))));
                lines.push(t.fg(color, &format!("    {}", diagnostic.message)));
            }
            None => lines.push(t.fg(color, &format!("  {}", diagnostic.message))),
        }
    }
    lines.join("\n")
}

impl InteractiveMode {
    fn add_loaded_section(
        &self,
        name: &'static str,
        collapsed: impl Fn() -> String + 'static,
        expanded: impl Fn() -> String + 'static,
    ) {
        let header = move || theme().fg(ThemeColor::MdHeading, &format!("[{name}]"));
        let section = ExpandableText::new(
            move || format!("{}\n{}", header(), collapsed()),
            move || format!("{}\n{}", header(), expanded()),
            self.startup_expanded(),
            0,
            0,
        );
        let mut container = self.ui.loaded_resources.borrow_mut();
        container.add_child(component(section));
        container.add_child(component(Spacer::new(1)));
    }

    fn add_diagnostics_section(&self, title: &'static str, text: String) {
        let mut container = self.ui.loaded_resources.borrow_mut();
        let build = move || format!("{}\n{text}", theme().fg(ThemeColor::Warning, title));
        container.add_child(component(ThemedText::new(build, 0, 0)));
        container.add_child(component(Spacer::new(1)));
    }

    /// The resource listing (unless quiet) and diagnostics (upstream `showLoadedResources`).
    pub(super) fn show_loaded_resources(&self, force: bool, diagnostics_when_quiet: bool) {
        self.ui.loaded_resources.borrow_mut().clear();
        let show_listing = force || self.should_show_startup_details();
        let show_diagnostics = show_listing || diagnostics_when_quiet;
        if !show_diagnostics {
            return;
        }
        let session = self.session();
        let cwd = session.cwd().to_path_buf();
        let resources = session.services().resources().resources().clone();
        let mut sources: HashMap<String, SourceInfo> = HashMap::new();
        let extensions: Vec<Item> = resources
            .extensions
            .iter()
            .map(|extension| Item { path: extension.path.display().to_string(), source: Some(extension.source_info.clone()) })
            .collect();
        for item in &extensions {
            if let Some(info) = &item.source {
                sources.insert(item.path.clone(), info.clone());
            }
        }
        for skill in &resources.skills {
            sources.insert(skill.file_path.display().to_string(), skill.source_info.clone());
        }
        for prompt in &resources.prompts {
            sources.insert(prompt.file_path.display().to_string(), prompt.source_info.clone());
        }
        for file in &resources.themes {
            sources.insert(file.path.display().to_string(), file.source_info.clone());
        }
        if show_listing {
            let context: Vec<String> = resources
                .system_prompt_source
                .iter()
                .chain(resources.append_system_prompt_sources.iter())
                .chain(resources.context_files.iter().map(|file| &file.path))
                .map(|path| path.display().to_string())
                .collect();
            if !context.is_empty() {
                self.ui.loaded_resources.borrow_mut().add_child(component(Spacer::new(1)));
                let relative: Vec<String> = context
                    .iter()
                    .map(|path| {
                        let absolute = resolve_lexically(Path::new(path), &cwd);
                        cwd_relative_path(&absolute, &cwd).unwrap_or_else(|| format_display_path(&absolute.display().to_string()))
                    })
                    .collect();
                let full = context.clone();
                self.add_loaded_section(
                    "Context",
                    move || compact_list(relative.clone(), false),
                    move || {
                        let lines: Vec<String> =
                            full.iter().map(|path| theme().fg(ThemeColor::Dim, &format!("  {}", format_display_path(path)))).collect();
                        lines.join("\n")
                    },
                );
            }
            if !resources.skills.is_empty() {
                let names: Vec<String> = resources.skills.iter().map(|skill| skill.name.clone()).collect();
                let items: Vec<Item> = resources
                    .skills
                    .iter()
                    .map(|skill| Item { path: skill.file_path.display().to_string(), source: Some(skill.source_info.clone()) })
                    .collect();
                self.add_loaded_section(
                    "Skills",
                    move || compact_list(names.clone(), true),
                    move || format_scope_groups(&items, &|item| format_display_path(&item.path)),
                );
            }
            if !resources.prompts.is_empty() {
                let names: Vec<String> = resources.prompts.iter().map(|prompt| format!("/{}", prompt.name)).collect();
                let by_path: HashMap<String, String> = resources
                    .prompts
                    .iter()
                    .map(|prompt| (prompt.file_path.display().to_string(), format!("/{}", prompt.name)))
                    .collect();
                let items: Vec<Item> = resources
                    .prompts
                    .iter()
                    .map(|prompt| Item { path: prompt.file_path.display().to_string(), source: Some(prompt.source_info.clone()) })
                    .collect();
                self.add_loaded_section(
                    "Prompts",
                    move || compact_list(names.clone(), true),
                    move || format_scope_groups(&items, &|item| by_path.get(&item.path).cloned().unwrap_or_else(|| format_display_path(&item.path))),
                );
            }
            if !extensions.is_empty() {
                let labels: Vec<String> = extensions.iter().map(|item| compact_extension_label(item, &extensions)).collect();
                let items = extensions.clone();
                self.add_loaded_section(
                    "Extensions",
                    move || compact_list(labels.clone(), true),
                    move || format_scope_groups(&items, &|item| format_display_path(&item.path)),
                );
            }
            self.add_mcp_servers_section();
        }
        let sections = [
            ("[Skill conflicts]", &resources.skill_diagnostics),
            ("[Prompt conflicts]", &resources.prompt_diagnostics),
            ("[Extension issues]", &resources.extension_diagnostics),
            ("[Theme conflicts]", &resources.theme_diagnostics),
        ];
        for (title, diagnostics) in sections {
            if !diagnostics.is_empty() {
                self.add_diagnostics_section(title, format_diagnostics(diagnostics, &sources));
            }
        }
    }
}
