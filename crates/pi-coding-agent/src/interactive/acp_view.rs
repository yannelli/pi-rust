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
use crate::acp_client::install::{install, uninstall};
use crate::acp_client::registry::{RegistryAgent, fetch};
use crate::acp_client::{AcpAgentEntry, AcpScope, platform_key, provider_id, registry_url, store, with_builtins};
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
            let url = registry_url();
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
                            change => match apply(change, &agents, &settings, &agent_dir).await {
                                Ok(message) => {
                                    crate::acp_client::providers::sync_providers(&mode.session()).await;
                                    message
                                }
                                Err(error) => format!("ACP agent change failed: {error}"),
                            },
                        };
                        let mut browser = view.borrow_mut();
                        browser.set_installed(installed_rows(&settings));
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

fn find(settings: &SettingsManager, id: &str) -> Option<(AcpAgentEntry, AcpScope)> {
    store::list(settings).into_iter().find(|(entry, _)| entry.id == id)
}

/// Install, enable or disable, or remove an agent; returns the status message.
async fn apply(
    action: AcpAction,
    agents: &[RegistryAgent],
    settings: &SettingsManager,
    agent_dir: &Path,
) -> Result<String, String> {
    match action {
        AcpAction::Install(id) => {
            let agent =
                agents.iter().find(|agent| agent.id == id).cloned().ok_or("The agent is not in the registry")?;
            let dir = agent_dir.to_path_buf();
            let entry =
                tokio::spawn(async move { install(&agent, &dir).await }).await.map_err(|error| error.to_string())??;
            store::upsert(settings, &entry, AcpScope::Global)?;
            Ok(format!("Installed {}. Its provider is {}.", entry.name, provider_id(&entry.id)))
        }
        AcpAction::Toggle(id) => {
            let (entry, scope) = find(settings, &id).ok_or("The agent is no longer installed")?;
            store::set_enabled(settings, &id, scope, !entry.enabled)?;
            Ok(format!("{} {}.", if entry.enabled { "Disabled" } else { "Enabled" }, entry.name))
        }
        AcpAction::Remove(id) => {
            let (entry, scope) = find(settings, &id).ok_or("The agent is no longer installed")?;
            store::remove(settings, &id, scope)?;
            let shared = store::list(settings).iter().any(|(other, _)| other.install_dir == entry.install_dir);
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
    store::list(settings)
        .into_iter()
        .map(|(entry, scope)| {
            let provider = provider_id(&entry.id);
            let command = std::iter::once(entry.command.as_str()).chain(entry.args.iter().map(String::as_str));
            AcpRow {
                is_default: default.as_deref() == Some(provider.as_str()),
                details: vec![
                    format!("Provider: {provider}"),
                    format!("Command: {}", command.collect::<Vec<_>>().join(" ")),
                    format!("Source: {}", entry.source),
                ],
                builtin: entry.source == "builtin",
                id: entry.id,
                name: entry.name,
                description: entry.description.unwrap_or_default(),
                version: entry.version.unwrap_or_default(),
                scope: scope.label().into(),
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
