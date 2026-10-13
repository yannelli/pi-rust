//! The hooks the extension host plugs into RPC mode (upstream `session.bindExtensions` in
//! `rpc-mode.ts`, `emitUserBash`, and registered commands for `get_commands`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::output::RpcOutput;
use super::types::RpcSlashCommand;
use super::ui::RpcExtensionUi;
use crate::agent_session::{AgentSession, AgentSessionRuntime, BashResult};
use crate::extension_ui::ExtensionMode;

/// An extension handler failure, sent as an `extension_error` record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionErrorRecord {
    pub extension_path: String,
    pub event: String,
    pub error: String,
}

/// Shutdown requested by an extension: RPC mode exits after the current command or once the
/// active run emits `agent_settled`.
#[derive(Clone, Default)]
pub struct RpcShutdownHandle {
    requested: Arc<AtomicBool>,
    now: CancellationToken,
}

impl RpcShutdownHandle {
    pub fn request(&self) {
        self.requested.store(true, Ordering::SeqCst);
    }

    pub fn is_requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }

    /// Start the shutdown when one was requested.
    pub(crate) fn check(&self) {
        if self.is_requested() {
            self.now.cancel();
        }
    }

    pub(crate) fn token(&self) -> &CancellationToken {
        &self.now
    }
}

/// What the extension host receives for each session RPC mode serves.
#[derive(Clone)]
pub struct RpcExtensionBinding {
    pub runtime: Arc<AgentSessionRuntime>,
    pub session: Arc<AgentSession>,
    /// The UI for `ctx.ui`.
    pub ui: Arc<RpcExtensionUi>,
    pub shutdown: RpcShutdownHandle,
    output: RpcOutput,
}

impl RpcExtensionBinding {
    pub(crate) fn new(
        runtime: Arc<AgentSessionRuntime>,
        session: Arc<AgentSession>,
        ui: Arc<RpcExtensionUi>,
        shutdown: RpcShutdownHandle,
        output: RpcOutput,
    ) -> Self {
        Self { runtime, session, ui, shutdown, output }
    }

    /// `ctx.mode`; `ctx.hasUI` is true in RPC mode.
    pub fn mode(&self) -> ExtensionMode {
        ExtensionMode::Rpc
    }

    /// Report a failed extension handler to the client.
    pub fn report_error(&self, error: &ExtensionErrorRecord) {
        self.output.send(&json!({
            "type": "extension_error",
            "extensionPath": error.extension_path,
            "event": error.event,
            "error": error.error,
        }));
    }
}

/// Implemented by the extension host to take part in RPC mode.
#[async_trait]
pub trait RpcExtensionHost: Send + Sync {
    /// Bind extensions to the first session and again after every session replacement.
    fn bind(&self, binding: RpcExtensionBinding);

    /// Registered extension commands; `get_commands` lists them first.
    fn commands(&self, _session: &AgentSession) -> Vec<RpcSlashCommand> {
        Vec::new()
    }

    /// The `user_bash` event: a result when an extension ran the command itself.
    async fn user_bash(&self, _session: &AgentSession, _command: &str, _exclude: bool) -> Option<BashResult> {
        None
    }
}
