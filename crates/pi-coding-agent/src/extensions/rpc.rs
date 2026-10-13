//! The extension host of RPC and ACP modes (upstream `rpc-mode.ts` `bindExtensions`): dialogs go
//! to the client, handler failures become `extension_error` records, and `ctx.shutdown()` ends
//! the mode once the current work settles.

use std::sync::Arc;

use async_trait::async_trait;

use super::runner::{ExtensionBindings, ExtensionError};
use crate::agent_session::{AgentSession, BashResult};
use crate::resources::SourceInfo;
use crate::rpc::{ExtensionErrorRecord, RpcCommandSource, RpcExtensionBinding, RpcExtensionHost, RpcSlashCommand};

/// Binds each session's extensions to the RPC (or ACP) client.
#[derive(Clone, Copy, Debug, Default)]
pub struct ExtensionRpcHost;

impl ExtensionRpcHost {
    pub fn shared() -> Arc<dyn RpcExtensionHost> {
        Arc::new(Self)
    }
}

#[async_trait]
impl RpcExtensionHost for ExtensionRpcHost {
    fn bind(&self, binding: RpcExtensionBinding) {
        let session = binding.session.clone();
        if session.extension_runner().is_none() {
            return;
        }
        session.mark_binding_extensions();
        let shutdown = binding.shutdown.clone();
        let reporter = binding.clone();
        let bindings = ExtensionBindings {
            ui: binding.ui.clone(),
            mode: binding.mode(),
            runtime: Some(Arc::downgrade(&binding.runtime)),
            shutdown: Some(Arc::new(move || shutdown.request())),
            abort: None,
            on_error: Some(Arc::new(move |error: &ExtensionError| {
                reporter.report_error(&ExtensionErrorRecord {
                    extension_path: error.extension_path.clone(),
                    event: error.event.clone(),
                    error: error.error.clone(),
                });
            })),
        };
        tokio::spawn(async move { session.bind_extensions(bindings).await });
    }

    fn commands(&self, session: &AgentSession) -> Vec<RpcSlashCommand> {
        let Some(runner) = session.extension_runner() else { return Vec::new() };
        let registry = runner.registry();
        registry
            .commands
            .iter()
            .filter_map(|command| {
                let source_info = serde_json::from_value::<SourceInfo>(command.source_info.clone()).ok()?;
                Some(RpcSlashCommand {
                    name: command.invocation_name.clone(),
                    description: command.description.clone(),
                    source: RpcCommandSource::Extension,
                    source_info,
                })
            })
            .collect()
    }

    async fn user_bash(&self, session: &AgentSession, command: &str, exclude: bool) -> Option<BashResult> {
        session.extension_user_bash(command, exclude).await
    }
}
