use anyhow::{bail, ensure};

use super::component::BrowserAction;
use super::data::{BrowserBackend, Target};
use crate::packages::{ConfigWriteScope, ParsedSource, parse_source};
use crate::resources::SourceScope;

impl BrowserBackend {
    pub async fn apply(&self, action: BrowserAction, target: Option<Target>) -> anyhow::Result<()> {
        let removing = matches!(action, BrowserAction::Remove(_));
        match action {
            BrowserAction::Install(source) => {
                let ParsedSource::Marketplace { name, tag } = parse_source(&source) else {
                    bail!("Select a marketplace extension to install");
                };
                self.marketplace.install(&name, tag.as_deref()).await?;
                self.manager.add_source_to_settings(&source, false)?;
            }
            BrowserAction::Toggle(_) => {
                let target = target.ok_or_else(|| anyhow::anyhow!("Extension no longer exists"))?;
                ensure!(!target.paths.is_empty(), "This package provides no extensions");
                let mut editor = self.editor().await?;
                if target.scope == SourceScope::Project {
                    editor.set_write_scope(ConfigWriteScope::Project);
                }
                for path in &target.paths {
                    editor.set_extension_enabled(path, !target.enabled).map_err(anyhow::Error::msg)?;
                }
            }
            BrowserAction::Remove(source) | BrowserAction::Update(source) => {
                let (source, scope) = match target {
                    Some(target) => {
                        ensure!(target.package, "Select the package row to update or remove it");
                        (target.source, target.scope)
                    }
                    None => (source, SourceScope::User),
                };
                ensure!(scope != SourceScope::Temporary, "This extension is temporary");
                if let ParsedSource::Marketplace { name, .. } = parse_source(&source) {
                    if removing {
                        self.marketplace.remove(&name)?;
                        self.manager.remove_source_from_settings(&source, scope == SourceScope::Project)?;
                    } else {
                        self.marketplace.update(&name).await?;
                    }
                } else if removing {
                    let source = if let ParsedSource::Local(local) = parse_source(&source) {
                        crate::resources::paths::resolve_path(&local.path, &self.manager.base_dir_for_scope(scope)?)
                            .display()
                            .to_string()
                    } else {
                        source
                    };
                    self.manager.remove_and_persist(&source, scope == SourceScope::Project).await?;
                } else {
                    ensure!(!self.manager.is_offline(), "Cannot update packages in offline mode");
                    self.manager.update(Some(&source)).await?;
                }
            }
            BrowserAction::Close | BrowserAction::Reload => {}
        }
        let errors = self.manager.settings().drain_errors();
        ensure!(errors.is_empty(), "{}", errors.iter().map(|error| error.diagnostic()).collect::<Vec<_>>().join("\n"));
        Ok(())
    }
}
