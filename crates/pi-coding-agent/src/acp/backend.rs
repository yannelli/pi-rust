//! What ACP mode needs from pi, in the shapes of pi's RPC commands (upstream drives a
//! `pi --mode rpc` child; here [`super::runtime::RuntimeBackend`] drives the session in-process).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_client_protocol_schema::{Error, RequestPermissionRequest, RequestPermissionResponse, SessionNotification};
use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// One pi session. Data methods return the `data` of the matching RPC command.
#[async_trait]
pub trait PiBackend: Send + Sync {
    /// Session events and `extension_ui_request` records as RPC JSON; taken once.
    fn take_events(&self) -> Option<UnboundedReceiver<Value>>;
    /// Start a prompt; returns once pi accepted it. Completion arrives as `agent_settled`.
    async fn prompt(&self, message: String, images: Vec<pi_ai::ContentBlock>) -> anyhow::Result<()>;
    async fn abort(&self) -> anyhow::Result<()>;
    async fn get_state(&self) -> anyhow::Result<Value>;
    async fn get_available_models(&self) -> anyhow::Result<Value>;
    async fn set_model(&self, provider: &str, model_id: &str) -> anyhow::Result<()>;
    async fn get_available_thinking_levels(&self) -> anyhow::Result<Vec<String>>;
    async fn set_thinking_level(&self, level: &str) -> anyhow::Result<()>;
    async fn set_steering_mode(&self, mode: &str) -> anyhow::Result<()>;
    async fn set_follow_up_mode(&self, mode: &str) -> anyhow::Result<()>;
    async fn compact(&self, custom_instructions: Option<String>) -> anyhow::Result<Value>;
    async fn set_auto_compaction(&self, enabled: bool) -> anyhow::Result<()>;
    async fn get_session_stats(&self) -> anyhow::Result<Value>;
    async fn set_session_name(&self, name: &str) -> anyhow::Result<()>;
    async fn export_html(&self, output_path: &Path) -> anyhow::Result<PathBuf>;
    async fn get_messages(&self) -> anyhow::Result<Value>;
    async fn get_commands(&self) -> anyhow::Result<Value>;
    /// Answer an `extension_ui_request` (`{id, value | confirmed | cancelled}`).
    async fn send_extension_ui_response(&self, response: Value) -> anyhow::Result<()>;
    /// Stop the session and release its resources.
    async fn dispose(&self);
}

/// Creates pi sessions: a new one in `cwd`, or the stored one at `session_file`.
#[async_trait]
pub trait BackendFactory: Send + Sync {
    async fn create(&self, cwd: &Path, session_file: Option<&Path>) -> Result<Arc<dyn PiBackend>, Error>;
}

/// The client side of the connection, as the agent calls it.
#[async_trait]
pub trait AcpClient: Send + Sync {
    /// Queue a `session/update` notification; notifications go out in call order.
    fn session_update(&self, notification: SessionNotification);
    async fn request_permission(&self, request: RequestPermissionRequest) -> Result<RequestPermissionResponse, Error>;
}
