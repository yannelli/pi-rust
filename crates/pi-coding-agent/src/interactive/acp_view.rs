//! `/acp`: the ACP agent browser, and `/acp mode`.

#[path = "acp_view_component/mod.rs"]
mod component;

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::Path;
use std::rc::Rc;

use tokio::sync::mpsc::unbounded_channel;

use self::component::{AcpAction, AcpBrowser, AcpRow};
use super::mode::InteractiveMode;
use crate::acp_client::install::{install, needs_program, uninstall};
use crate::acp_client::registry::registry_url_for;
use crate::acp_client::registry::{RegistryAgent, fetch};
use crate::acp_client::{AcpAgentEntry, AcpScope, platform_key, provider_id, store, with_builtins};
use crate::settings::SettingsManager;

impl InteractiveMode {
    pub(super) fn handle_acp_command(&self, args: Option<String>) {
        match args.as_deref() {
            Some("mode") => self.acp_mode(),
            _ => self.show_acp_browser(),
        }
    }

    fn show_acp_browser(&self) {
        let (send, mut receive) = unbounded_channel();
        let weak = self.weak.clone();
        let view = Rc::new(RefCell::new(AcpBrowser::new(installed_rows(&self.settings()), move |action| {
            let _ = send.send(action);
            if let Some(mode) = weak.upgrade() {
                mode.request_render();
            }
        })));
        let close = self.mount_view(view.clone());
        let agent_dir = self.agent_dir.clone();
        self.spawn(move |mode| async move {
            let url = registry_url_for(&mode.settings());
            let mut fetching = Some(tokio::spawn(async move { fetch(&url).await }));
            let mut agents: Vec<RegistryAgent> = Vec::new();
            let mut registry_error = None;
            loop {
                tokio::select! {
                    result = async { fetching.as_mut().expect("active fetch").await }, if fetching.is_some() => {
                        fetching = None;
                        let fetched = match result {
                            Ok(Ok(fetched)) => fetched,
                            Ok(Err(error)) => { registry_error = Some(error); Vec::new() }
                            Err(error) => { registry_error = Some(error.to_string()); Vec::new() }
                        };
                        agents = with_builtins(fetched);
                        view.borrow_mut().set_registry(registry_rows(&agents, &mode.settings()), registry_error.clone());
                        mode.request_render();
                    }
                    action = receive.recv() => {
                        let Some(action) = action else { break };
                        let settings = mode.settings();
                        let saving = matches!(&action, AcpAction::Save { .. });
                        let mut save_error = None;
                        let status = match action {
                            AcpAction::Close => { close(); break; }
                            AcpAction::Login(id) => {
                                let Some((entry, _)) = find(&settings, &id) else { continue };
                                close();
                                mode.acp_login(entry);
                                break;
                            }
                            AcpAction::SetDefault(id) => {
                                if let Some((entry, _)) = find(&settings, &id) {
                                    mode.acp_set_default(entry);
                                }
                                String::new()
                            }
                            AcpAction::Install(id, scope) if program_missing(&agents, &id) => {
                                let Some(agent) = agents.iter().find(|agent| agent.id == id).cloned() else { continue };
                                close();
                                mode.acp_install_program(agent, scope);
                                break;
                            }
                            change => match apply_and_sync(change, &agents, &settings, &agent_dir, || async {
                                crate::acp_client::providers::sync_providers(&mode.session()).await;
                            }).await {
                                Ok(message) => message,
                                Err(error) => {
                                    if saving {
                                        save_error = Some(error.clone());
                                    }
                                    format!("ACP agent change failed: {error}")
                                }
                            },
                        };
                        let mut browser = view.borrow_mut();
                        browser.set_installed(installed_rows(&settings));
                        if saving {
                            browser.finish_save(save_error);
                        }
                        if fetching.is_none() {
                            browser.set_registry(registry_rows(&agents, &settings), registry_error.clone());
                        }
                        browser.set_status(status, false);
                        drop(browser);
                        mode.request_render();
                    }
                }
            }
            if let Some(fetching) = fetching {
                fetching.abort();
            }
        });
    }
}

/// Whether agent `id` runs a program that is missing and that has an installer pi can run.
fn program_missing(agents: &[RegistryAgent], id: &str) -> bool {
    let path = std::env::var_os("PATH");
    let agent = agents.iter().find(|agent| agent.id == id);
    agent.is_some_and(|agent| {
        agent.local.as_ref().is_some_and(|local| !local.installer.is_empty()) && needs_program(agent, path.as_deref())
    })
}

fn find(settings: &SettingsManager, id: &str) -> Option<(AcpAgentEntry, AcpScope)> {
    store::list(settings).into_iter().find(|(entry, _)| entry.id == id)
}

async fn apply_and_sync<F: std::future::Future<Output = ()>>(
    action: AcpAction,
    agents: &[RegistryAgent],
    settings: &SettingsManager,
    agent_dir: &Path,
    sync: impl FnOnce() -> F,
) -> Result<String, String> {
    let message = apply(action, agents, settings, agent_dir).await?;
    sync().await;
    Ok(message)
}

/// Apply a settings change and return its status message.
async fn apply(
    action: AcpAction,
    agents: &[RegistryAgent],
    settings: &SettingsManager,
    agent_dir: &Path,
) -> Result<String, String> {
    match action {
        AcpAction::Save { entry, scope, editing } => {
            if editing {
                if store::get(settings, &entry.id, scope).is_none() {
                    return Err("The agent is no longer installed".into());
                }
            } else if store::contains_id(settings, &entry.id) {
                return Err(format!("ID \"{}\" is already installed. Choose another ID.", entry.id));
            }
            let provider = provider_id(&entry.id);
            if let Some((other, _, _)) = store::all(settings)
                .into_iter()
                .find(|(other, _, _)| other.id != entry.id && provider_id(&other.id) == provider)
            {
                return Err(format!("Provider \"{provider}\" already belongs to {}. Choose another ID.", other.name));
            }
            store::upsert(settings, &entry, scope)?;
            Ok(format!(
                "Saved {} in the {} settings. Its provider is {}.",
                entry.name,
                scope.label(),
                provider_id(&entry.id)
            ))
        }
        AcpAction::Install(id, scope) => {
            let agent =
                agents.iter().find(|agent| agent.id == id).cloned().ok_or("The agent is not in the registry")?;
            let dir = agent_dir.to_path_buf();
            let entry =
                tokio::spawn(async move { install(&agent, &dir).await }).await.map_err(|error| error.to_string())??;
            store::upsert(settings, &entry, scope)?;
            let (name, provider, scope) = (entry.name, provider_id(&entry.id), scope.label());
            Ok(format!("Installed {name} in the {scope} settings. Its provider is {provider}."))
        }
        AcpAction::Toggle(id, scope) => {
            let entry = store::get(settings, &id, scope).ok_or("The agent is no longer installed")?;
            store::set_enabled(settings, &id, scope, !entry.enabled)?;
            Ok(format!("{} {}.", if entry.enabled { "Disabled" } else { "Enabled" }, entry.name))
        }
        AcpAction::Move(id, scope) => {
            let entry = store::get(settings, &id, scope).ok_or("The agent is no longer installed")?;
            let to = store::move_entry(settings, &id, scope)?;
            Ok(format!("Moved {} to the {} settings.", entry.name, to.label()))
        }
        AcpAction::Remove(id, scope) => {
            let entry = store::get(settings, &id, scope).ok_or("The agent is no longer installed")?;
            store::remove(settings, &id, scope)?;
            let shared = store::all(settings).iter().any(|(other, _, _)| other.install_dir == entry.install_dir);
            if !shared {
                uninstall(&entry, agent_dir)?;
            }
            Ok(format!("Removed {}.", entry.name))
        }
        AcpAction::Close | AcpAction::Login(_) | AcpAction::SetDefault(_) => Ok(String::new()),
    }
}

fn installed_rows(settings: &SettingsManager) -> Vec<AcpRow> {
    let default = settings.settings().default_provider;
    store::all(settings)
        .into_iter()
        .map(|(entry, scope, shadowed)| {
            let provider = provider_id(&entry.id);
            let command = std::iter::once(entry.command.as_str()).chain(entry.args.iter().map(String::as_str));
            let mut details = vec![
                format!("Provider: {provider}"),
                format!("Command: {}", command.collect::<Vec<_>>().join(" ")),
                format!("Source: {}", entry.source),
            ];
            details.extend(entry.env.keys().map(|key| format!("{key}=********")));
            AcpRow {
                is_default: !shadowed && default.as_deref() == Some(provider.as_str()),
                details,
                entry: Some(entry.clone()),
                builtin: entry.source == "builtin",
                id: entry.id,
                name: entry.name,
                description: entry.description.unwrap_or_default(),
                version: entry.version.unwrap_or_default(),
                scope: Some(scope),
                shadowed,
                enabled: Some(entry.enabled),
                installed: true,
            }
        })
        .collect()
}

fn registry_rows(agents: &[RegistryAgent], settings: &SettingsManager) -> Vec<AcpRow> {
    let installed: HashSet<String> = store::list(settings).into_iter().map(|(entry, _)| entry.id).collect();
    agents
        .iter()
        .map(|agent| AcpRow {
            id: agent.id.clone(),
            name: agent.name.clone(),
            description: agent.description.clone(),
            version: agent.version.clone(),
            installed: installed.contains(&agent.id),
            builtin: agent.builtin,
            details: vec![format!("Install: {}", distribution_label(agent))],
            ..AcpRow::default()
        })
        .collect()
}

fn distribution_label(agent: &RegistryAgent) -> String {
    let distribution = &agent.distribution;
    if let Some(key) = platform_key().filter(|key| distribution.binary.contains_key(*key)) {
        format!("binary for {key}")
    } else if let Some(npx) = &distribution.npx {
        format!("npx {}", npx.package)
    } else if let Some(uvx) = &distribution.uvx {
        format!("uvx {}", uvx.package)
    } else if let Some(local) = &agent.local {
        format!("{} on PATH", local.program)
    } else {
        "none for this platform".into()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use pi_tui::Component;
    use serde_json::json;

    use super::*;
    use crate::interactive_components::test_support::{Calls, keys, render_text, setup};

    #[tokio::test]
    async fn add_custom_flow_saves_and_syncs_providers() {
        let _keys = setup();
        let settings = SettingsManager::in_memory(json!({}));
        let calls = Calls::new();
        let mut browser = AcpBrowser::new(installed_rows(&settings), calls.recorder());
        browser.handle_input(keys::ENTER);
        browser.handle_input("My Custom Agent");
        browser.handle_input(keys::TAB);
        browser.handle_input(keys::TAB);
        browser.handle_input("agent-cli");
        browser.handle_input(keys::TAB);
        browser.handle_input("acp --agent-engine=v3 'two words'");
        browser.handle_input(keys::TAB);
        browser.handle_input("agent-cli login --label 'my account'");
        browser.handle_input(keys::TAB);
        browser.handle_input("A custom agent");
        browser.handle_input(keys::TAB);
        browser.handle_input("\x1b[200~TOKEN=secret\n");
        browser.handle_input("REGION=us east\x1b[201~");
        browser.handle_input(keys::TAB);
        browser.handle_input(" ");
        browser.handle_input("\x13");
        let synced = Cell::new(0);
        let dir = tempfile::tempdir().unwrap();
        let action = calls.take().pop().expect("save action");
        apply_and_sync(action, &[], &settings, dir.path(), || async { synced.set(synced.get() + 1) }).await.unwrap();
        assert_eq!(synced.get(), 1);
        let entry = store::get(&settings, "my-custom-agent", AcpScope::Project).unwrap();
        assert_eq!(entry.args, ["acp", "--agent-engine=v3", "two words"]);
        assert_eq!(entry.login_command.unwrap(), ["agent-cli", "login", "--label", "my account"]);
        assert_eq!(entry.env["TOKEN"], "secret");
        assert_eq!(entry.env["REGION"], "us east");
        assert_eq!(entry.description.as_deref(), Some("A custom agent"));
        assert_eq!(entry.source, "custom");
        assert!(entry.enabled);
        assert!(store::get(&settings, "my-custom-agent", AcpScope::Global).is_none());
        browser.set_installed(installed_rows(&settings));
        browser.finish_save(None);
        browser.set_status(String::new(), false);
        let text = render_text(&mut browser, 120);
        assert!(text.contains("My Custom Agent") && text.contains("[project]"), "{text}");
    }

    #[tokio::test]
    async fn edit_keeps_scope_metadata_and_unknown_fields() {
        let _keys = setup();
        let original = json!({"id": "agent", "name": "Installed Agent", "command": "old-cli",
            "source": "registry", "version": "1.2.3", "installDir": "/agents/agent/1.2.3",
            "description": "Old description", "loginCommand": ["old-cli", "login"],
            "futureField": {"keep": true}});
        let settings = SettingsManager::in_memory(json!({"acpAgents": [original]}));
        store::move_entry(&settings, "agent", AcpScope::Global).unwrap();
        let calls = Calls::new();
        let mut browser = AcpBrowser::new(installed_rows(&settings), calls.recorder());
        browser.handle_input(keys::ENTER);
        browser.handle_input(keys::UP);
        browser.handle_input(keys::UP);
        browser.handle_input(keys::ENTER);
        let text = render_text(&mut browser, 120);
        assert!(text.contains("ID (read-only): agent") && text.contains("Scope: project (read-only)"), "{text}");
        browser.handle_input("\x0b");
        browser.handle_input("Renamed Agent");
        browser.handle_input(keys::TAB);
        browser.handle_input("\x0b");
        browser.handle_input("new-cli");
        browser.handle_input(keys::TAB);
        browser.handle_input(keys::TAB);
        browser.handle_input("\x0b");
        browser.handle_input(keys::TAB);
        browser.handle_input("\x0b");
        browser.handle_input(keys::TAB);
        browser.handle_input(keys::TAB);
        browser.handle_input(" ");
        browser.handle_input("\x13");
        let dir = tempfile::tempdir().unwrap();
        apply_and_sync(calls.take().pop().unwrap(), &[], &settings, dir.path(), || async {}).await.unwrap();
        assert!(store::get(&settings, "agent", AcpScope::Global).is_none());
        let saved = settings.project_settings().extra["acpAgents"][0].clone();
        assert_eq!(saved["name"], "Renamed Agent");
        assert_eq!(saved["command"], "new-cli");
        assert_eq!(saved["enabled"], false);
        assert_eq!(saved["source"], "registry");
        assert_eq!(saved["version"], "1.2.3");
        assert_eq!(saved["installDir"], "/agents/agent/1.2.3");
        assert_eq!(saved["futureField"], json!({"keep": true}));
        assert!(saved.get("description").is_none());
        assert!(saved.get("loginCommand").is_none());
    }

    #[test]
    fn details_mask_environment_values() {
        let _keys = setup();
        let settings = SettingsManager::in_memory(json!({"acpAgents": [{
            "id": "agent", "name": "Agent", "command": "agent-cli",
            "env": {"TOKEN": "secret-token", "REGION": "private-region"}
        }]}));
        let mut browser = AcpBrowser::new(installed_rows(&settings), |_| {});
        browser.handle_input(keys::ENTER);
        let text = render_text(&mut browser, 120);
        assert!(text.contains("TOKEN=********") && text.contains("REGION=********"), "{text}");
        assert!(!text.contains("secret-token") && !text.contains("private-region"), "{text}");
    }
}
