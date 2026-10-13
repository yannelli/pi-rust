//! Typed command methods of [`RpcClient`] (upstream `RpcClient` command methods and helpers).

use std::time::Duration;

use pi_agent::QueueMode;
use pi_ai::{ContentBlock, Message, Model, ThinkingLevel};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedReceiver;

use super::client::RpcClient;
use super::client_types::{
    RpcClearedQueue, RpcCompactionResult, RpcEntries, RpcForkMessage, RpcForkResult, RpcModelCycle, RpcSessionStats,
};
use super::tree::RpcSessionTree;
use super::types::{RpcBashResult, RpcCommand, RpcSessionState, RpcSlashCommand, RpcStreamingBehavior};
use crate::agent_session::PromptDisposition;

fn decode<T: DeserializeOwned>(value: Value) -> anyhow::Result<T> {
    serde_json::from_value(value).map_err(|error| anyhow::anyhow!("Unexpected response data: {error}"))
}

fn field<T: DeserializeOwned>(mut data: Value, key: &str) -> anyhow::Result<T> {
    decode(data.get_mut(key).map(Value::take).unwrap_or(Value::Null))
}

fn disposition(data: Value) -> anyhow::Result<PromptDisposition> {
    match field::<String>(data, "disposition")?.as_str() {
        "handled" => Ok(PromptDisposition::Handled),
        "queued" => Ok(PromptDisposition::Queued),
        "started" => Ok(PromptDisposition::Started),
        other => anyhow::bail!("Unexpected prompt disposition: {other}"),
    }
}

fn images_option(images: Vec<ContentBlock>) -> Option<Vec<ContentBlock>> {
    (!images.is_empty()).then_some(images)
}

/// Read `events` until `agent_settled`; the records up to and including it.
pub async fn collect_until_settled(
    events: &mut UnboundedReceiver<Value>,
    timeout: Duration,
) -> anyhow::Result<Vec<Value>> {
    let mut collected = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv()).await;
        let event = match event {
            Ok(Some(event)) => event,
            Ok(None) => anyhow::bail!("Event stream closed before agent_settled"),
            Err(_) => anyhow::bail!("Timeout collecting events"),
        };
        let settled = event.get("type").and_then(Value::as_str) == Some("agent_settled");
        collected.push(event);
        if settled {
            return Ok(collected);
        }
    }
}

impl RpcClient {
    /// Send a prompt; the disposition says whether it started a run, was queued, or handled.
    pub async fn prompt(
        &self,
        message: &str,
        images: Vec<ContentBlock>,
        streaming_behavior: Option<RpcStreamingBehavior>,
    ) -> anyhow::Result<PromptDisposition> {
        let images = images_option(images);
        disposition(self.request(RpcCommand::Prompt { message: message.into(), images, streaming_behavior }).await?)
    }

    pub async fn steer(&self, message: &str, images: Vec<ContentBlock>) -> anyhow::Result<PromptDisposition> {
        disposition(self.request(RpcCommand::Steer { message: message.into(), images: images_option(images) }).await?)
    }

    pub async fn follow_up(&self, message: &str, images: Vec<ContentBlock>) -> anyhow::Result<PromptDisposition> {
        let images = images_option(images);
        disposition(self.request(RpcCommand::FollowUp { message: message.into(), images }).await?)
    }

    pub async fn abort(&self) -> anyhow::Result<()> {
        self.request(RpcCommand::Abort).await.map(drop)
    }

    pub async fn clear_queue(&self) -> anyhow::Result<RpcClearedQueue> {
        decode(self.request(RpcCommand::ClearQueue).await?)
    }

    /// Start a new session; true when an extension cancelled it.
    pub async fn new_session(&self, parent_session: Option<&str>) -> anyhow::Result<bool> {
        let parent_session = parent_session.map(String::from);
        field(self.request(RpcCommand::NewSession { parent_session }).await?, "cancelled")
    }

    pub async fn get_state(&self) -> anyhow::Result<RpcSessionState> {
        decode(self.request(RpcCommand::GetState).await?)
    }

    pub async fn set_model(&self, provider: &str, model_id: &str) -> anyhow::Result<Model> {
        decode(self.request(RpcCommand::SetModel { provider: provider.into(), model_id: model_id.into() }).await?)
    }

    /// The next model, or `None` when there is only one.
    pub async fn cycle_model(&self) -> anyhow::Result<Option<RpcModelCycle>> {
        decode(self.request(RpcCommand::CycleModel).await?)
    }

    pub async fn get_available_models(&self) -> anyhow::Result<Vec<Model>> {
        field(self.request(RpcCommand::GetAvailableModels).await?, "models")
    }

    pub async fn set_thinking_level(&self, level: ThinkingLevel) -> anyhow::Result<()> {
        self.request(RpcCommand::SetThinkingLevel { level }).await.map(drop)
    }

    /// The next level, or `None` when the model has no reasoning.
    pub async fn cycle_thinking_level(&self) -> anyhow::Result<Option<ThinkingLevel>> {
        let data = self.request(RpcCommand::CycleThinkingLevel).await?;
        if data.is_null() { Ok(None) } else { field(data, "level") }
    }

    pub async fn get_available_thinking_levels(&self) -> anyhow::Result<Vec<ThinkingLevel>> {
        field(self.request(RpcCommand::GetAvailableThinkingLevels).await?, "levels")
    }

    pub async fn set_steering_mode(&self, mode: QueueMode) -> anyhow::Result<()> {
        self.request(RpcCommand::SetSteeringMode { mode }).await.map(drop)
    }

    pub async fn set_follow_up_mode(&self, mode: QueueMode) -> anyhow::Result<()> {
        self.request(RpcCommand::SetFollowUpMode { mode }).await.map(drop)
    }

    pub async fn compact(&self, custom_instructions: Option<&str>) -> anyhow::Result<RpcCompactionResult> {
        let custom_instructions = custom_instructions.map(String::from);
        decode(self.request(RpcCommand::Compact { custom_instructions }).await?)
    }

    pub async fn set_auto_compaction(&self, enabled: bool) -> anyhow::Result<()> {
        self.request(RpcCommand::SetAutoCompaction { enabled }).await.map(drop)
    }

    pub async fn set_auto_retry(&self, enabled: bool) -> anyhow::Result<()> {
        self.request(RpcCommand::SetAutoRetry { enabled }).await.map(drop)
    }

    pub async fn abort_retry(&self) -> anyhow::Result<()> {
        self.request(RpcCommand::AbortRetry).await.map(drop)
    }

    /// Run a shell command and add its output to the context of the next prompt.
    pub async fn bash(&self, command: &str) -> anyhow::Result<RpcBashResult> {
        self.bash_with(command, false).await
    }

    /// Run a shell command; `exclude_from_context` keeps its output away from the model.
    pub async fn bash_with(&self, command: &str, exclude_from_context: bool) -> anyhow::Result<RpcBashResult> {
        let exclude_from_context = exclude_from_context.then_some(true);
        decode(self.request(RpcCommand::Bash { command: command.into(), exclude_from_context }).await?)
    }

    pub async fn abort_bash(&self) -> anyhow::Result<()> {
        self.request(RpcCommand::AbortBash).await.map(drop)
    }

    pub async fn get_session_stats(&self) -> anyhow::Result<RpcSessionStats> {
        decode(self.request(RpcCommand::GetSessionStats).await?)
    }

    /// Export the session to HTML; returns the written path.
    pub async fn export_html(&self, output_path: Option<&str>) -> anyhow::Result<String> {
        let output_path = output_path.map(String::from);
        field(self.request(RpcCommand::ExportHtml { output_path }).await?, "path")
    }

    /// Open another session file; true when an extension cancelled the switch.
    pub async fn switch_session(&self, session_path: &str) -> anyhow::Result<bool> {
        field(self.request(RpcCommand::SwitchSession { session_path: session_path.into() }).await?, "cancelled")
    }

    pub async fn fork(&self, entry_id: &str) -> anyhow::Result<RpcForkResult> {
        decode(self.request(RpcCommand::Fork { entry_id: entry_id.into() }).await?)
    }

    /// Copy the active branch into a new session; true when an extension cancelled it.
    pub async fn clone_session(&self) -> anyhow::Result<bool> {
        field(self.request(RpcCommand::CloneSession).await?, "cancelled")
    }

    pub async fn get_fork_messages(&self) -> anyhow::Result<Vec<RpcForkMessage>> {
        field(self.request(RpcCommand::GetForkMessages).await?, "messages")
    }

    /// Entries in append order, only those after `since` when given.
    pub async fn get_entries(&self, since: Option<&str>) -> anyhow::Result<RpcEntries> {
        decode(self.request(RpcCommand::GetEntries { since: since.map(String::from) }).await?)
    }

    pub async fn get_tree(&self) -> anyhow::Result<RpcSessionTree> {
        RpcSessionTree::from_data(self.request(RpcCommand::GetTree).await?).map_err(anyhow::Error::msg)
    }

    pub async fn get_last_assistant_text(&self) -> anyhow::Result<Option<String>> {
        field(self.request(RpcCommand::GetLastAssistantText).await?, "text")
    }

    pub async fn set_session_name(&self, name: &str) -> anyhow::Result<()> {
        self.request(RpcCommand::SetSessionName { name: name.into() }).await.map(drop)
    }

    pub async fn get_messages(&self) -> anyhow::Result<Vec<Message>> {
        field(self.request(RpcCommand::GetMessages).await?, "messages")
    }

    /// Extension commands, prompt templates, and skills a prompt can run as `/name`.
    pub async fn get_commands(&self) -> anyhow::Result<Vec<RpcSlashCommand>> {
        field(self.request(RpcCommand::GetCommands).await?, "commands")
    }

    /// Wait for the next `agent_settled`; call it only while a run is active.
    pub async fn wait_for_idle(&self, timeout: Duration) -> anyhow::Result<()> {
        let mut events = self.subscribe();
        collect_until_settled(&mut events, timeout).await.map(drop)
    }

    /// Prompt and return its events through `agent_settled` (none when it was handled).
    pub async fn prompt_and_wait(
        &self,
        message: &str,
        images: Vec<ContentBlock>,
        timeout: Duration,
    ) -> anyhow::Result<Vec<Value>> {
        let mut events = self.subscribe();
        if self.prompt(message, images, None).await? == PromptDisposition::Handled {
            return Ok(Vec::new());
        }
        collect_until_settled(&mut events, timeout).await
    }
}
