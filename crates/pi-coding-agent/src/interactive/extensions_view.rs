#[path = "extensions_view_actions.rs"]
mod actions;
#[path = "extensions_view_component/mod.rs"]
mod component;
#[path = "extensions_view_data.rs"]
mod data;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use tokio::sync::mpsc::unbounded_channel;

use self::component::{BrowserAction, ExtensionsBrowser};
use self::data::{BrowserBackend, Snapshot, mark_updates};
use super::mode::InteractiveMode;
use crate::agent_session::BUILTIN_EXTENSIONS;
use crate::packages::{Marketplace, PackageManager, PackageManagerOptions};

impl InteractiveMode {
    pub(super) fn handle_extensions_command(&self) {
        let session = self.session();
        let mut options = PackageManagerOptions::new(session.cwd(), &self.agent_dir, self.settings());
        options.builtin_extensions = BUILTIN_EXTENSIONS.iter().map(|name| (*name).into()).collect();
        let manager = PackageManager::new(options);
        let marketplace = manager.marketplace();
        self.show_extensions_browser(manager, marketplace);
    }

    pub(super) fn show_extensions_browser(&self, manager: PackageManager, marketplace: Marketplace) {
        let backend = Arc::new(BrowserBackend { manager, marketplace, session: self.session() });
        let (send, mut receive) = unbounded_channel();
        let weak = self.weak.clone();
        let view = Rc::new(RefCell::new(ExtensionsBrowser::new(Vec::new(), move |action| {
            let _ = send.send(action);
            if let Some(mode) = weak.upgrade() {
                mode.request_render();
            }
        })));
        view.borrow_mut().set_status("Loading installed extensions...".into(), true);
        let close = self.mount_view(view.clone());
        self.spawn(move |mode| async move {
            let worker = backend.clone();
            let mut snapshot = match tokio::spawn(async move { worker.snapshot().await }).await {
                Ok(Ok(snapshot)) => {
                    view.borrow_mut().set_status(String::new(), false);
                    snapshot
                }
                result => {
                    view.borrow_mut().set_status(format!("Could not list extensions: {}", result_error(result)), false);
                    Snapshot { rows: Vec::new(), targets: Default::default() }
                }
            };
            view.borrow_mut().set_installed(snapshot.rows.clone());
            mode.request_render();
            let market = backend.clone();
            let mut fetch = Some(tokio::spawn(async move { market.marketplace.browse(None).await.map(|catalog| catalog.index) }));
            let mut index = None;
            let mut catalog = Vec::new();
            loop {
                tokio::select! {
                    result = async { fetch.as_mut().expect("active fetch").await }, if fetch.is_some() => {
                        fetch = None;
                        match result {
                            Ok(Ok(fetched)) => {
                                let worker = backend.clone();
                                let current = fetched.clone();
                                match tokio::spawn(async move { worker.catalog_rows(&current) }).await {
                                    Ok(Ok(rows)) => {
                                        catalog = rows;
                                        mark_updates(&mut snapshot, &catalog);
                                        view.borrow_mut().set_installed(snapshot.rows.clone());
                                        view.borrow_mut().set_catalog(catalog.clone(), None);
                                    }
                                    result => view.borrow_mut().set_catalog(Vec::new(), Some(result_error(result))),
                                }
                                index = Some(fetched);
                            }
                            result => view.borrow_mut().set_catalog(Vec::new(), Some(result_error(result))),
                        }
                        mode.request_render();
                    }
                    action = receive.recv() => {
                        let Some(action) = action else { break };
                        match &action {
                            BrowserAction::Close => { close(); break; }
                            BrowserAction::Reload => {
                                close();
                                mode.handle_reload_command();
                                break;
                            }
                            _ => {}
                        }
                        let id = match &action {
                            BrowserAction::Install(id) | BrowserAction::Update(id) |
                            BrowserAction::Remove(id) | BrowserAction::Toggle(id) => id,
                            _ => unreachable!(),
                        };
                        let target = snapshot.targets.get(id).or_else(|| {
                            snapshot.targets.values().find(|target| target.package && (target.source == *id || target.source.strip_prefix(id.as_str()).is_some_and(|suffix| suffix.starts_with('@'))))
                        }).cloned();
                        view.borrow_mut().set_status("Applying extension change...".into(), true);
                        mode.request_render();
                        let worker = backend.clone();
                        let current_index = index.clone();
                        let result = tokio::spawn(async move {
                            let changed = worker.apply(action, target).await;
                            let snapshot = worker.snapshot().await;
                            let catalog = current_index.as_ref().map(|index| worker.catalog_rows(index)).transpose();
                            (changed, snapshot, catalog)
                        }).await;
                        match result {
                            Ok((changed, loaded, rows)) => {
                                let catalog_error = match rows {
                                    Ok(Some(rows)) => { catalog = rows; None }
                                    Ok(None) => None,
                                    Err(error) => Some(format!("Could not refresh marketplace: {error:#}")),
                                };
                                let load_error = match loaded {
                                    Ok(mut loaded) => {
                                        mark_updates(&mut loaded, &catalog);
                                        view.borrow_mut().set_installed(loaded.rows.clone());
                                        snapshot = loaded;
                                        None
                                    }
                                    Err(error) => Some(format!("Could not refresh extensions: {error:#}")),
                                };
                                if index.is_some() { view.borrow_mut().set_catalog(catalog.clone(), None); }
                                match changed {
                                    Ok(None) => {
                                        view.borrow_mut().set_status(load_error.or(catalog_error).unwrap_or_else(|| "Extension change saved.".into()), false);
                                        view.borrow_mut().offer_reload();
                                    }
                                    Ok(Some(message)) => view.borrow_mut().set_status(load_error.or(catalog_error).unwrap_or(message), false),
                                    Err(error) => {
                                        view.borrow_mut().set_status(format!("Extension change failed: {error:#}"), false);
                                    }
                                }
                            }
                            Err(error) => view.borrow_mut().set_status(format!("Extension task failed: {error}"), false),
                        }
                        mode.request_render();
                    }
                }
            }
            if let Some(fetch) = fetch { fetch.abort(); }
        });
    }
}

fn result_error<T>(result: Result<anyhow::Result<T>, tokio::task::JoinError>) -> String {
    match result {
        Ok(Err(error)) => format!("{error:#}"),
        Err(error) => error.to_string(),
        Ok(Ok(_)) => String::new(),
    }
}

#[cfg(test)]
#[path = "../../tests/interactive/extensions_browser.rs"]
pub(super) mod tests;
