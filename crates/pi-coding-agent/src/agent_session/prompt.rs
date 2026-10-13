//! Prompting (upstream `prompt`, `steer`, `followUp`, `sendCustomMessage`, `clearQueue`):
//! skill and template expansion, queueing while streaming, auth checks, image normalization,
//! and the system prompt update sent with the next request.

use std::sync::Arc;

use pi_ai::{ContentBlock, Message, UserContent, UserMessage};

use super::builtins::mcp::set_servers_section;
use super::{AgentSession, lock};
use crate::app::{format_no_api_key_found_message, oauth_failed_message};
use crate::images::{ProcessImageOptions, decode_base64_lenient, process_image};
use crate::messages::{CustomMessage, CustomRole};
use crate::resources::expand_user_input;

/// How a prompt sent while streaming is queued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamingBehavior {
    /// Delivered after the current turn's tool calls, before the next request.
    Steer,
    /// Delivered when the agent would otherwise stop.
    FollowUp,
}

/// What happened to an accepted prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptDisposition {
    /// Consumed without a run (an extension command or input handler).
    Handled,
    /// Queued as steering or a follow-up.
    Queued,
    /// A run started.
    Started,
}

impl PromptDisposition {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Handled => "handled",
            Self::Queued => "queued",
            Self::Started => "started",
        }
    }
}

/// Options of [`AgentSession::prompt_with`].
#[derive(Clone)]
pub struct PromptOptions {
    /// Expand `/skill:name` and prompt templates (default true).
    pub expand_prompt_templates: bool,
    pub images: Vec<ContentBlock>,
    /// Required while streaming.
    pub streaming_behavior: Option<StreamingBehavior>,
    /// Called once the prompt is accepted, before the run, with its disposition.
    pub preflight_result: Option<Arc<dyn Fn(PromptDisposition) + Send + Sync>>,
}

impl Default for PromptOptions {
    fn default() -> Self {
        Self { expand_prompt_templates: true, images: Vec::new(), streaming_behavior: None, preflight_result: None }
    }
}

/// The arguments of a `/mcp` command line, or `None` for other input.
fn mcp_command_args(text: &str) -> Option<&str> {
    let rest = text.trim().strip_prefix("/mcp")?;
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then(|| rest.trim())
}

fn user_message(text: &str, images: Vec<ContentBlock>) -> Message {
    let mut blocks = vec![ContentBlock::text(text)];
    blocks.extend(images);
    Message::User(UserMessage { content: UserContent::Blocks(blocks), timestamp: pi_ai::now_ms() })
}

impl AgentSession {
    /// Send a prompt and wait until the run settles (retries and queued work included).
    pub async fn prompt(&self, text: &str, images: Vec<ContentBlock>) -> anyhow::Result<PromptDisposition> {
        self.prompt_with(text, PromptOptions { images, ..PromptOptions::default() }).await
    }

    pub async fn prompt_with(&self, text: &str, options: PromptOptions) -> anyhow::Result<PromptDisposition> {
        if lock(&self.state).manual_compaction.is_some() {
            anyhow::bail!(
                "Cannot submit a prompt while compaction is in progress. Wait for compaction to finish and retry."
            );
        }
        let preflight = |disposition| {
            if let Some(callback) = &options.preflight_result {
                callback(disposition);
            }
        };
        if options.expand_prompt_templates
            && let Some(args) = mcp_command_args(text)
            && self.mcp_manager().is_some()
        {
            preflight(PromptDisposition::Handled);
            self.run_mcp_command(args).await;
            return Ok(PromptDisposition::Handled);
        }
        let expanded = if options.expand_prompt_templates { self.expand_input(text) } else { text.to_string() };
        if self.is_streaming() {
            let Some(behavior) = options.streaming_behavior else {
                anyhow::bail!(
                    "Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message."
                );
            };
            self.queue_message(&expanded, options.images, behavior);
            preflight(PromptDisposition::Queued);
            return Ok(PromptDisposition::Queued);
        }
        self.flush_pending_bash_messages();
        self.flush_pending_custom_messages();
        self.ensure_auth().await?;
        let last_assistant = self.with_state(|state| {
            state.messages.iter().rev().find_map(|message| match message {
                Message::Assistant(assistant) => Some(assistant.clone()),
                _ => None,
            })
        });
        if let Some(last) = last_assistant {
            self.check_compaction(&last, None, false, &[]).await;
        }
        let (images, hints) = self.normalize_prompt_images(&options.images);
        let text = if hints.is_empty() { expanded } else { format!("{expanded}\n\n{}", hints.join("\n")) };
        let mut messages = vec![user_message(&text, images)];
        messages.append(&mut lock(&self.state).pending_next_turn);
        let servers_section = self.prepare_mcp_for_prompt().await;
        let mut prompt_options = self.effective_prompt_options();
        set_servers_section(&mut prompt_options, servers_section);
        let update = self.prepare_prompt_and_tool_loadout(&prompt_options, &self.messages());
        lock(&self.state).run_prompt = Some(prompt_options);
        if let Some(update) = update {
            messages.insert(0, Message::System(update));
        }
        preflight(PromptDisposition::Started);
        self.run_agent_prompt(messages).await?;
        Ok(PromptDisposition::Started)
    }

    /// `/skill:name args` and `/template args` expansion; on an error the text is sent as is.
    pub fn expand_input(&self, text: &str) -> String {
        let services = self.services();
        let loader = services.resources();
        let resources = loader.resources();
        match expand_user_input(text, &resources.skills, &resources.prompts) {
            Ok(expanded) => expanded,
            Err(error) => {
                eprintln!("Warning: {error:#}");
                text.to_string()
            }
        }
    }

    async fn ensure_auth(&self) -> anyhow::Result<()> {
        let model = self.model().ok_or_else(|| anyhow::anyhow!(super::compaction::no_model_selected_message()))?;
        let runtime = self.model_runtime();
        if runtime.has_configured_auth(&model.provider) || runtime.check_auth(&model.provider).await {
            return Ok(());
        }
        if runtime.is_using_oauth(&model.provider).await {
            anyhow::bail!(oauth_failed_message(&model.provider));
        }
        anyhow::bail!(format_no_api_key_found_message(&model.provider))
    }

    /// Resize prompt images per `images.autoResize`; unusable images become hints.
    fn normalize_prompt_images(&self, images: &[ContentBlock]) -> (Vec<ContentBlock>, Vec<String>) {
        let auto_resize = self.settings().image_auto_resize();
        let options = match self.model() {
            Some(model) => ProcessImageOptions::for_model(&model, auto_resize),
            None => ProcessImageOptions { auto_resize, ..ProcessImageOptions::default() },
        };
        let mut normalized = Vec::new();
        let mut hints = Vec::new();
        for image in images {
            let ContentBlock::Image { data, mime_type } = image else {
                normalized.push(image.clone());
                continue;
            };
            match process_image(&decode_base64_lenient(data), mime_type, &options) {
                Ok(processed) => {
                    normalized.push(ContentBlock::Image { data: processed.data, mime_type: processed.mime_type });
                    hints.extend(processed.hints);
                }
                Err(hint) => hints.push(hint),
            }
        }
        (normalized, hints)
    }

    /// Queue a steering message, delivered after the current turn's tool calls.
    pub fn steer(&self, text: &str, images: Vec<ContentBlock>) -> PromptDisposition {
        let expanded = self.expand_input(text);
        self.queue_message(&expanded, images, StreamingBehavior::Steer);
        PromptDisposition::Queued
    }

    /// Queue a follow-up, delivered when the agent has no more tool calls or steering.
    pub fn follow_up(&self, text: &str, images: Vec<ContentBlock>) -> PromptDisposition {
        let expanded = self.expand_input(text);
        self.queue_message(&expanded, images, StreamingBehavior::FollowUp);
        PromptDisposition::Queued
    }

    fn queue_message(&self, text: &str, images: Vec<ContentBlock>, behavior: StreamingBehavior) {
        {
            let mut state = lock(&self.state);
            match behavior {
                StreamingBehavior::Steer => state.steering.push(text.to_string()),
                StreamingBehavior::FollowUp => state.follow_up.push(text.to_string()),
            }
        }
        self.emit_queue_update();
        let message = user_message(text, images);
        match behavior {
            StreamingBehavior::Steer => self.agent.steer(message),
            StreamingBehavior::FollowUp => self.agent.follow_up(message),
        }
    }

    /// Clear both queues and return their texts (steering, follow-up).
    pub fn clear_queue(&self) -> (Vec<String>, Vec<String>) {
        let cleared = {
            let mut state = lock(&self.state);
            (std::mem::take(&mut state.steering), std::mem::take(&mut state.follow_up))
        };
        self.agent.clear_queues();
        self.emit_queue_update();
        cleared
    }

    /// Steering and follow-up messages not delivered yet.
    pub fn pending_message_count(&self) -> usize {
        let state = lock(&self.state);
        state.steering.len() + state.follow_up.len()
    }

    pub fn steering_messages(&self) -> Vec<String> {
        lock(&self.state).steering.clone()
    }

    pub fn follow_up_messages(&self) -> Vec<String> {
        lock(&self.state).follow_up.clone()
    }

    /// Add a `custom` message to the conversation. While streaming it waits for the end of the
    /// current turn (or is queued when `trigger_turn`); otherwise `trigger_turn` starts a run.
    pub async fn send_custom_message(&self, message: CustomMessage, trigger_turn: bool) -> anyhow::Result<()> {
        let message = message.into_message();
        if self.is_streaming() {
            if trigger_turn {
                self.agent.steer(message);
            } else {
                lock(&self.state).pending_custom.push(message);
            }
            return Ok(());
        }
        if trigger_turn {
            return self.run_agent_prompt(vec![message]).await;
        }
        self.append_custom_message(message);
        Ok(())
    }

    /// Context for the next user prompt, sent along with it.
    pub fn queue_next_turn_message(&self, message: CustomMessage) {
        lock(&self.state).pending_next_turn.push(message.into_message());
    }
}
