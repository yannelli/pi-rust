use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use super::component::ExtensionRow;
use crate::agent_session::AgentSession;
use crate::packages::marketplace::is_newer_tag;
use crate::packages::{
    ConfigEditor, ConfigWriteScope, Marketplace, MarketplaceIndex, MissingSourceAction, MissingSourceHandler,
    PackageManager, PackageManagerOptions, ScopedResolvedPaths,
};
use crate::resources::{ResourceType, SourceOrigin, SourceScope};
use crate::settings::SettingsManager;

pub(super) struct BrowserBackend {
    pub manager: PackageManager,
    pub marketplace: Marketplace,
    pub session: Arc<AgentSession>,
}

#[derive(Clone)]
pub(super) struct Target {
    pub source: String,
    pub scope: SourceScope,
    pub paths: Vec<String>,
    pub enabled: bool,
    pub package: bool,
}

pub(super) struct Snapshot {
    pub rows: Vec<ExtensionRow>,
    pub targets: HashMap<String, Target>,
}

impl BrowserBackend {
    pub async fn editor(&self) -> anyhow::Result<ConfigEditor> {
        let manager = &self.manager;
        let globals = Arc::new(SettingsManager::in_memory(serde_json::to_value(manager.settings.global_settings())?));
        let mut options = PackageManagerOptions::new(&manager.cwd, &manager.agent_dir, globals);
        options.home = manager.home.clone();
        options.runner = manager.runner.clone();
        options.builtin_extensions = manager.builtin_extensions.clone();
        options.offline = Some(true);
        let skip: MissingSourceHandler = Arc::new(|_| Box::pin(async { MissingSourceAction::Skip }));
        let global = PackageManager::new(options).resolve(Some(&skip)).await?;
        let project = manager.resolve(Some(&skip)).await?;
        Ok(ConfigEditor::new(
            &ScopedResolvedPaths { global, project },
            manager.settings.clone(),
            &manager.cwd,
            &manager.agent_dir,
            &manager.home,
            ConfigWriteScope::Global,
        ))
    }

    pub async fn snapshot(&self) -> anyhow::Result<Snapshot> {
        let mut editor = self.editor().await?;
        if self.manager.settings.is_project_trusted() {
            editor.set_write_scope(ConfigWriteScope::Project);
        }
        let items: Vec<_> = editor
            .groups()
            .iter()
            .flat_map(|group| &group.subgroups)
            .filter(|subgroup| subgroup.kind == ResourceType::Extensions)
            .flat_map(|subgroup| subgroup.items.clone())
            .collect();
        let mut snapshot = Snapshot { rows: Vec::new(), targets: HashMap::new() };
        for package in self.manager.list_configured_packages().await {
            let id = format!("package:{:?}:{}", package.scope, package.source);
            let provided: Vec<_> = items
                .iter()
                .filter(|item| {
                    item.metadata.origin == SourceOrigin::Package
                        && item.metadata.source == package.source
                        && item.metadata.scope == package.scope
                })
                .collect();
            let manifest = package.installed_path.as_deref().map(read_manifest).unwrap_or_default();
            let mut row = ExtensionRow {
                id: id.clone(),
                name: package.source.clone(),
                description: manifest["description"].as_str().unwrap_or_default().into(),
                source: format!("{} ({:?})", package.source, package.scope),
                version: manifest["version"].as_str().unwrap_or_default().into(),
                path: package.installed_path.as_ref().map(|path| path.display().to_string()).unwrap_or_default(),
                tags: Vec::new(),
                enabled: (!provided.is_empty()).then(|| provided.iter().any(|item| item.enabled)),
                installed: true,
                update_available: false,
                details: Vec::new(),
            };
            for item in &provided {
                row.details.push(format!("Extension: {}", item.path.display()));
                row.details.extend(self.registrations(&item.path.to_string_lossy()));
            }
            snapshot.targets.insert(
                id,
                Target {
                    source: package.source,
                    scope: package.scope,
                    paths: provided.iter().map(|item| item.path.display().to_string()).collect(),
                    enabled: row.enabled.unwrap_or(false),
                    package: true,
                },
            );
            snapshot.rows.push(row);
        }
        for item in items {
            let path = item.path.display().to_string();
            let id = format!("extension:{:?}:{path}", item.metadata.scope);
            let manifest = item.metadata.package_root.as_deref().map(read_manifest).unwrap_or_default();
            snapshot.rows.push(ExtensionRow {
                id: id.clone(),
                name: format!("  {}", item.display_name),
                description: manifest["description"].as_str().unwrap_or_default().into(),
                source: format!("{} ({:?})", item.metadata.source, item.metadata.scope),
                version: if item.metadata.source == "builtin" {
                    env!("CARGO_PKG_VERSION").into()
                } else {
                    manifest["version"].as_str().unwrap_or_default().into()
                },
                path: path.clone(),
                tags: Vec::new(),
                enabled: Some(item.enabled),
                installed: true,
                update_available: false,
                details: self.registrations(&path),
            });
            snapshot.targets.insert(
                id,
                Target {
                    source: item.metadata.source,
                    scope: item.metadata.scope,
                    paths: vec![path],
                    enabled: item.enabled,
                    package: false,
                },
            );
        }
        Ok(snapshot)
    }

    fn registrations(&self, path: &str) -> Vec<String> {
        let mut details = Vec::new();
        if let Some(runner) = self.session.extension_runner() {
            details.extend(
                runner.registry().commands.iter().filter(|command| command.source_info["path"] == path).map(
                    |command| {
                        format!(
                            "Command: /{} {}",
                            command.invocation_name,
                            command.description.as_deref().unwrap_or_default()
                        )
                    },
                ),
            );
        }
        details.extend(
            self.session
                .all_tools()
                .iter()
                .filter(|tool| tool.source_path == path)
                .map(|tool| format!("Tool: {} {}", tool.name, tool.description)),
        );
        let command = match path {
            "builtin:mcp" => Some("mcp"),
            "builtin:llama.cpp" => Some("llama"),
            _ => None,
        };
        if let Some(command) = command {
            details.push(format!("Command: /{command}"));
        }
        details
    }

    pub fn catalog_rows(&self, index: &MarketplaceIndex) -> anyhow::Result<Vec<ExtensionRow>> {
        let installed = self.marketplace.installed()?;
        Ok(index
            .extensions
            .iter()
            .map(|entry| {
                let package = installed.iter().find(|package| package.name == entry.name);
                let update_available =
                    package.is_some_and(|package| !package.pinned && is_newer_tag(&index.tag, &package.tag));
                ExtensionRow {
                    id: format!("marketplace:{}", entry.name),
                    name: format!("{} ({})", entry.display_name, entry.name),
                    description: entry.description.clone(),
                    source: format!("marketplace:{}", entry.name),
                    version: entry.version.clone(),
                    path: package.map(|package| package.path.display().to_string()).unwrap_or_default(),
                    tags: entry.tags.clone(),
                    enabled: None,
                    installed: package.is_some(),
                    update_available,
                    details: vec![
                        format!("Release: {}", index.tag),
                        format!("Platforms: {}", entry.platforms.join(", ")),
                    ],
                }
            })
            .collect())
    }
}

pub(super) fn mark_updates(snapshot: &mut Snapshot, catalog: &[ExtensionRow]) {
    for row in &mut snapshot.rows {
        if let Some(target) = snapshot.targets.get(&row.id) {
            row.update_available = catalog.iter().any(|entry| target.source == entry.source && entry.update_available);
        }
    }
}

fn read_manifest(path: &Path) -> serde_json::Value {
    std::fs::read(path.join("package.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}
